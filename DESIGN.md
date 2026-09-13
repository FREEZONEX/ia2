# IA2 product design

IA2 is a working environment for writing control logic, connecting devices and
observing a running controller. The interface follows Tier0 Product: a compact
industrial workbench with clear actions, readable values and visible failures.
The HTTP API remains the authority for behavior.

## Visual language

Use white and neutral gray surfaces, near-black primary actions, 4 px control
radii and restrained separators. Establish hierarchy through spacing, alignment
and type before adding borders. Avoid nested cards, decorative gradients,
uppercase micro-labels and oversized empty states.

Body text is 13 px; supporting text is 12 px. Use IBM Plex Sans for interface
text and IBM Plex Mono for source code, identifiers, addresses and measured
values. The source editor uses 14 px type with 22 px line height. Standard table
rows are 36 px. Controls must retain visible keyboard focus and useful names.

Carbon is the product icon family, exposed through the shared icon module.
Bundle icons, Plex font files, Monaco and its worker with the application.
Opening the editor or HMI must not require a CDN. CJK text uses available local
font fallbacks; the presence of a font in the fallback stack does not mean it
is bundled.

## Workbench and state

The project explorer organizes sources, tasks, devices, mappings and screens.
The center is the selected editor or configuration view. Monitor sits below it,
initially around one third of the available height. Both panels have visible
collapse controls and remember their sizes. The collapsed Monitor keeps a
40 px status header and continues receiving live status.

Use one pane header for the title, context and primary action. Configuration
forms use aligned fields; channel and variable lists use tables. Empty states
explain the next useful action and fit inside a small pane. Search narrows
variables without concealing controller health. Graphical editors share the
same typography, selection and command conventions as the text editor.

Lime indicates selection or agent activity. Runtime health uses separate green,
amber and red signals. A selected tab is not a healthy controller. Running,
paused, stepping, disconnected, stale and faulted states must remain distinct.
Show device failures and watchdog output locks explicitly.

Pause, resume, step, force and alarm acknowledgment show pending feedback until
the server responds. A failed request keeps the last confirmed state and shows
its reason. Preserve REAL fractions and reject invalid numeric input; Escape
cancels a draft without writing. Unsaved source changes require a discard
decision before navigation replaces them. Runtime operations retain their
existing safety rules.

## HMI

The IDE and standalone operator panel share the canvas and action behavior.
Keep Fit, 100% and zoom controls outside the scrolling document. Fit considers
both available dimensions so the entire authored screen is visible. At 100%,
the document retains its authored size and the viewport scrolls. Trends,
legends and axes stay inside their assigned node.

Operate enables live actions and locks layout. Arrange enables layout editing
and disables operator actions. Changing mode or screen clears outstanding
action confirmations. Confirmations describe the actual typed write, start
keyboard focus on Cancel and support Escape. A layout save or alarm
acknowledgment failure remains visible; the interface must not invent success.

## Themes and native surfaces

Light is the default. One persisted preference controls the IDE and standalone
HMI, including same-origin windows. Dark mode changes surfaces, text, controls,
charts, scrollbars and editor colors while preserving state meanings.

Where the Windows desktop host is present, its background and title bar follow
the applied web theme, with a fixed caption color matching the page surface.
The native caption owns the application icon and name; the web content beneath
it uses a 40 px project toolbar with 8 px gaps and does not repeat that branding.
The browser interface retains its own IA2 branding and 48 px navigation bar.
The native system caption remains separate and keeps standard window controls.
Startup and error pages have self-contained styles so
they remain readable when the backend is unavailable. Closing a native window
keeps its controller running in the background; application exit retains the
running-controller protection.

## Sources

The implementation uses the Product surface of
[Tier0 Design System, commit 52e01e9](https://github.com/FREEZONEX/Tier0-Design-System/tree/52e01e94c18188668f47dfe7664f27cfde1690f6):
`SKILL.md`, `DESIGN.md`, `surfaces/tier0-product/README.md` and
`sources/spec.product-ui.md`.

Concrete theme values were checked against
[Tier0 Frontend, commit 9073779](https://github.com/FREEZONEX/Tier0-Frontend/tree/9073779711ee2201ae3207489a45ef96055dee9b),
especially `packages/theme/src/themes.scss`, `variables.scss`, `tailwind.css`
and `token.ts`. IA2 maps those roles onto its existing components and reserves
separate colors for controller health. Repository API and safety contracts take
precedence over visual examples.
