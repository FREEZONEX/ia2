//! Alarm supervision: what the runtime does when it can no longer evaluate
//! alarms.
//!
//! The alarm engine, the history and the live feed all hang off one task
//! (`tap`). When it stops, is stuck, or keeps failing, the alarm list goes
//! quiet because nothing is looking, and a quiet list reads as "nothing is
//! wrong". `tap` reports that state (`snapshot_tap` on `/health` and
//! `/status`); this module watches it and acts on the project's declared
//! policy (`[supervision]` in `alarms.toml`):
//!
//! - `report` (the default): say so, and keep running. Stopping a plant is not
//!   always the safe direction, so the runtime never decides to on its own.
//! - `safe_state`: once alarm evaluation has been lost for the grace period,
//!   latch the outputs off exactly as the scan watchdog does (every device into
//!   failsafe, no more output writes) until a person restarts the program.
//!   Never undone automatically, and never done by restarting the process.
//!
//! Logging is deliberately not proportional to time. Losing alarm evaluation is
//! logged at ERROR the moment it is seen; reminders then back off
//! exponentially (1, 2, 4, 8 ... minutes into the episode, at most hourly), so
//! a day-long outage is a dozen lines, not one a minute. The state itself is
//! always in `snapshot_tap`, for whoever is not reading the journal.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ironplc_bridge::monitor::SnapshotTapHealth;
use ironplc_bridge::ProgramHandle;
use project::{SupervisionAction, SupervisionConfig};

use crate::tap::TapStatus;

/// A fault of the snapshot task this recent still counts as "failing now". Keep
/// in step with `ALARM_FAULT_RECENT_MS` in the operator panel
/// (`apps/web/src/components/hmi/panel-health.ts`).
const RECENT_FAULT_MS: u64 = 10_000;
/// How often the task is looked at.
const TICK: Duration = Duration::from_secs(1);
/// Into an episode, when the first reminder is due.
const FIRST_REMINDER: Duration = Duration::from_secs(60);
/// The longest gap between reminders.
const MAX_REMINDER_GAP: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    pub on_loss: SupervisionAction,
    pub grace: Duration,
}

impl From<&SupervisionConfig> for Policy {
    fn from(c: &SupervisionConfig) -> Self {
        Self {
            on_loss: c.on_loss,
            grace: Duration::from_secs(u64::from(c.grace_secs)),
        }
    }
}

impl Policy {
    /// One line for the startup log and for the loss message, so the journal
    /// says what will happen without a trip to the project files.
    pub fn describe(&self) -> String {
        match self.on_loss {
            SupervisionAction::Report => {
                "on_loss = report: no automatic action is configured".to_string()
            }
            SupervisionAction::SafeState => format!(
                "on_loss = safe_state: the outputs are latched off if alarm evaluation \
                 stays lost for {} s",
                self.grace.as_secs()
            ),
        }
    }
}

/// Why alarm evaluation is lost right now; `None` while it is not.
pub fn assess(h: &SnapshotTapHealth) -> Option<String> {
    if !h.running {
        return Some("the snapshot task has stopped".to_string());
    }
    if let Some(ms) = h.stalled_for_ms {
        return Some(format!(
            "the snapshot task has been stuck inside one snapshot for {}.{} s",
            ms / 1000,
            (ms % 1000) / 100
        ));
    }
    match h.last_fault_age_ms {
        Some(age) if age < RECENT_FAULT_MS => Some(format!(
            "the snapshot task faulted {} s ago{}",
            (age / 1000).max(1),
            h.last_fault
                .as_deref()
                .map(|t| format!(" ({t})"))
                .unwrap_or_default()
        )),
        _ => None,
    }
}

/// What a step of the supervisor asks its caller to do.
#[derive(Debug, PartialEq)]
pub enum Action {
    /// Alarm evaluation was just lost: ERROR, immediately.
    Lost { why: String },
    /// Still lost, at one of the exponentially spaced reminders.
    StillLost { why: String, for_secs: u64 },
    /// It is back.
    Restored { after_secs: u64 },
    /// `safe_state` and the grace ran out: latch the outputs off.
    LatchOff { reason: String },
}

/// The supervisor's memory between looks. Pure: the caller passes the time and
/// what it saw, so the grace period and the log schedule are testable.
pub struct Supervisor {
    policy: Policy,
    lost_since: Option<Instant>,
    /// Lost episodes so far. Only the 1st, 2nd, 4th, 8th ... are announced, so
    /// a task that flaps cannot flood the journal.
    episodes: u64,
    announced: bool,
    next_reminder: Duration,
    latch_requested: bool,
}

impl Supervisor {
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            lost_since: None,
            episodes: 0,
            announced: false,
            next_reminder: FIRST_REMINDER,
            latch_requested: false,
        }
    }

    /// One look. `lost` is [`assess`]'s answer; `outputs_latched` is whether the
    /// outputs are already latched off (by the scan watchdog, say), in which
    /// case there is nothing left to ask for.
    pub fn step(
        &mut self,
        now: Instant,
        lost: Option<String>,
        outputs_latched: bool,
    ) -> Vec<Action> {
        let mut actions = Vec::new();
        match lost {
            Some(why) => {
                let since = *self.lost_since.get_or_insert_with(|| {
                    self.episodes += 1;
                    self.announced = self.episodes.is_power_of_two();
                    self.next_reminder = FIRST_REMINDER;
                    if self.announced {
                        actions.push(Action::Lost { why: why.clone() });
                    }
                    now
                });
                let elapsed = now.saturating_duration_since(since);
                if self.announced && elapsed >= self.next_reminder && elapsed > Duration::ZERO {
                    actions.push(Action::StillLost {
                        why: why.clone(),
                        for_secs: elapsed.as_secs(),
                    });
                    self.next_reminder = (elapsed * 2).min(elapsed + MAX_REMINDER_GAP);
                }
                if self.policy.on_loss == SupervisionAction::SafeState
                    && !self.latch_requested
                    && !outputs_latched
                    && elapsed >= self.policy.grace
                {
                    self.latch_requested = true;
                    actions.push(Action::LatchOff {
                        reason: format!(
                            "alarm supervision lost for {} s: {why}",
                            self.policy.grace.as_secs()
                        ),
                    });
                }
            }
            None => {
                if let Some(since) = self.lost_since.take() {
                    if self.announced {
                        actions.push(Action::Restored {
                            after_secs: now.saturating_duration_since(since).as_secs(),
                        });
                    }
                    self.announced = false;
                }
            }
        }
        actions
    }
}

/// Watch the snapshot task for the life of the process.
pub async fn supervise(tap: Arc<TapStatus>, handle: ProgramHandle, policy: Policy) {
    supervise_every(TICK, tap, handle, policy).await
}

pub async fn supervise_every(
    tick: Duration,
    tap: Arc<TapStatus>,
    handle: ProgramHandle,
    policy: Policy,
) {
    let mut supervisor = Supervisor::new(policy.clone());
    let mut interval = tokio::time::interval(tick);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let lost = assess(&tap.health());
        for action in supervisor.step(Instant::now(), lost, handle.watchdog_tripped()) {
            carry_out(action, &policy, &handle).await;
        }
    }
}

async fn carry_out(action: Action, policy: &Policy, handle: &ProgramHandle) {
    match action {
        Action::Lost { why } => tracing::error!(
            %why,
            policy = %policy.describe(),
            "alarm supervision LOST: {why}. Process alarms are not being evaluated and \
             history and the live feed may have stopped; a quiet alarm list is not an \
             all-clear. {}",
            policy.describe()
        ),
        Action::StillLost { why, for_secs } => tracing::error!(
            %why,
            for_secs,
            "alarm supervision still lost after {for_secs} s: {why} (reminders back off \
             exponentially; the state is in /health and /status `snapshot_tap`)"
        ),
        Action::Restored { after_secs } => {
            tracing::info!(
                after_secs,
                "alarm supervision restored after {after_secs} s"
            )
        }
        Action::LatchOff { reason } => match handle.latch_outputs_off(reason.clone()).await {
            Ok(true) => tracing::error!(
                %reason,
                "alarm supervision policy latched the outputs off; restart the program \
                 to re-arm"
            ),
            Ok(false) => tracing::warn!("outputs were already latched off"),
            Err(e) => tracing::error!(
                %reason, %e,
                "alarm supervision policy could not latch the outputs off: the scan loop \
                 is gone"
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn health() -> SnapshotTapHealth {
        SnapshotTapHealth {
            running: true,
            faults: 0,
            last_fault: None,
            stalled_for_ms: None,
            last_fault_age_ms: None,
        }
    }

    fn policy(on_loss: SupervisionAction, grace_secs: u64) -> Policy {
        Policy {
            on_loss,
            grace: Duration::from_secs(grace_secs),
        }
    }

    fn lost() -> Option<String> {
        Some("the snapshot task has stopped".to_string())
    }

    #[test]
    fn assess_names_a_stopped_a_stuck_and_a_recently_faulting_task() {
        assert_eq!(assess(&health()), None);
        assert!(assess(&SnapshotTapHealth {
            running: false,
            ..health()
        })
        .unwrap()
        .contains("has stopped"));
        let stuck = assess(&SnapshotTapHealth {
            stalled_for_ms: Some(4200),
            ..health()
        })
        .unwrap();
        assert!(stuck.contains("4.2 s"), "{stuck}");
        let recent = assess(&SnapshotTapHealth {
            faults: 3,
            last_fault: Some("the alarms stage panicked on scan 9".into()),
            last_fault_age_ms: Some(1500),
            ..health()
        })
        .unwrap();
        assert!(
            recent.contains("faulted 1 s ago") && recent.contains("alarms stage"),
            "{recent}"
        );
    }

    #[test]
    fn assess_ignores_an_old_fault() {
        let old = SnapshotTapHealth {
            faults: 1,
            last_fault: Some("x".into()),
            last_fault_age_ms: Some(RECENT_FAULT_MS),
            ..health()
        };
        assert_eq!(
            assess(&old),
            None,
            "history, not a task that is failing now"
        );
    }

    /// The journal is not proportional to time: ERROR the moment it is lost,
    /// reminders at 1, 2, 4, 8 ... minutes into the episode and at most hourly
    /// after that, one line when it is back.
    #[test]
    fn the_log_schedule_backs_off_instead_of_repeating_every_minute() {
        let mut s = Supervisor::new(policy(SupervisionAction::Report, 30));
        let t0 = Instant::now();
        let mut reminders = Vec::new();
        let first = s.step(t0, lost(), false);
        assert_eq!(
            first,
            vec![Action::Lost {
                why: "the snapshot task has stopped".into()
            }]
        );
        // A full day, looked at once a second.
        let mut lines = 1;
        for sec in 1..=86_400u64 {
            for a in s.step(t0 + Duration::from_secs(sec), lost(), false) {
                if let Action::StillLost { for_secs, .. } = a {
                    reminders.push(for_secs);
                    lines += 1;
                }
            }
        }
        assert_eq!(&reminders[..4], &[60, 120, 240, 480], "{reminders:?}");
        assert!(
            lines <= 30,
            "a day-long outage must be a handful of lines, not {lines}: {reminders:?}"
        );
        // Never closer together than the cap allows once it is hourly.
        assert!(reminders
            .windows(2)
            .rev()
            .take(5)
            .all(|w| w[1] - w[0] <= 3600 + 1));
        let back = s.step(t0 + Duration::from_secs(86_401), None, false);
        assert_eq!(back, vec![Action::Restored { after_secs: 86_401 }]);
    }

    #[test]
    fn report_never_asks_for_the_outputs_to_be_latched() {
        let mut s = Supervisor::new(policy(SupervisionAction::Report, 30));
        let t0 = Instant::now();
        for sec in 0..=3600u64 {
            let acts = s.step(t0 + Duration::from_secs(sec), lost(), false);
            assert!(
                !acts.iter().any(|a| matches!(a, Action::LatchOff { .. })),
                "report must never latch: {acts:?}"
            );
        }
    }

    #[test]
    fn safe_state_latches_once_when_the_grace_runs_out_and_never_again() {
        let mut s = Supervisor::new(policy(SupervisionAction::SafeState, 30));
        let t0 = Instant::now();
        let mut latch_at = Vec::new();
        for sec in 0..=120u64 {
            for a in s.step(t0 + Duration::from_secs(sec), lost(), false) {
                if let Action::LatchOff { reason } = a {
                    assert!(reason.contains("lost for 30 s"), "{reason}");
                    latch_at.push(sec);
                }
            }
        }
        assert_eq!(latch_at, vec![30], "exactly once, at the end of the grace");

        // Recovering afterwards announces it and asks for nothing: leaving the
        // latched state is for a person, not for the supervisor.
        let back = s.step(t0 + Duration::from_secs(121), None, true);
        assert!(
            back.iter().all(|a| matches!(a, Action::Restored { .. })),
            "{back:?}"
        );
        let again = s.step(t0 + Duration::from_secs(300), lost(), true);
        assert!(
            !again.iter().any(|a| matches!(a, Action::LatchOff { .. })),
            "{again:?}"
        );
    }

    /// A tap that comes back inside the grace is a hiccup, and the clock starts
    /// over for the next loss.
    #[test]
    fn a_recovery_inside_the_grace_resets_it() {
        let mut s = Supervisor::new(policy(SupervisionAction::SafeState, 30));
        let t0 = Instant::now();
        let mut latched = false;
        let mut look = |sec: u64, lost: Option<String>| {
            for a in s.step(t0 + Duration::from_secs(sec), lost, false) {
                latched |= matches!(a, Action::LatchOff { .. });
            }
        };
        for sec in 0..=20 {
            look(sec, lost());
        }
        look(21, None);
        for sec in 22..=45 {
            look(sec, lost());
        }
        assert!(
            !latched,
            "20 s lost, a good look, then 24 s lost is never 30 s in a row"
        );
    }

    #[test]
    fn nothing_is_asked_when_the_scan_watchdog_already_latched_the_outputs() {
        let mut s = Supervisor::new(policy(SupervisionAction::SafeState, 5));
        let t0 = Instant::now();
        for sec in 0..=60 {
            let acts = s.step(t0 + Duration::from_secs(sec), lost(), true);
            assert!(
                !acts.iter().any(|a| matches!(a, Action::LatchOff { .. })),
                "{acts:?}"
            );
        }
    }

    /// A task that flaps must not flood the journal: only the 1st, 2nd, 4th,
    /// 8th ... episode is announced (and only an announced one is closed with
    /// a "restored" line).
    #[test]
    fn a_flapping_task_is_announced_logarithmically() {
        let mut s = Supervisor::new(policy(SupervisionAction::Report, 30));
        let t0 = Instant::now();
        let (mut lost_lines, mut restored_lines) = (0, 0);
        for episode in 0..64u64 {
            let at = |sec: u64| t0 + Duration::from_secs(episode * 10 + sec);
            for a in s.step(at(0), lost(), false) {
                lost_lines += matches!(a, Action::Lost { .. }) as u32;
            }
            for a in s.step(at(5), None, false) {
                restored_lines += matches!(a, Action::Restored { .. }) as u32;
            }
        }
        assert_eq!(lost_lines, 7, "episodes 1, 2, 4, 8, 16, 32, 64");
        assert_eq!(restored_lines, lost_lines, "every announced loss is closed");
    }

    #[test]
    fn the_policy_comes_from_the_project_and_defaults_to_stopping_nothing() {
        let default = Policy::from(&SupervisionConfig::default());
        assert_eq!(default.on_loss, SupervisionAction::Report);
        assert!(default.describe().contains("no automatic action"));
        let declared = Policy::from(&SupervisionConfig {
            on_loss: SupervisionAction::SafeState,
            grace_secs: 45,
        });
        assert_eq!(declared.grace, Duration::from_secs(45));
        assert!(
            declared.describe().contains("latched off") && declared.describe().contains("45 s")
        );
    }
}
