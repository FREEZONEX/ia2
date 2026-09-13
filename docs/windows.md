# Native Windows desktop

The target is **Windows 11 x64 (`x86_64-pc-windows-msvc`)**. `IA2.exe`
opens the IDE in a native window using Microsoft Edge WebView2. The
server, CLI, language-server sidecar and runtime remain separate native
executables; WSL is not needed. A successful build alone is not acceptance: run the
native gate and installed-artifact workflow before reporting verification.
See the [recorded native validation](windows-validation.md) for the tested
environment, results and remaining hardware boundaries.

| Capability | Windows scope |
| --- | --- |
| Desktop IDE, ST/LD/FBD/SFC compilation, language server | Native window and local server; WebView2 Runtime required |
| Simulation, monitor, alarms, history, HMI | Native server/runtime; scenario evidence required |
| Modbus TCP, OPC UA, MQTT | Native implementations; physical integration requires a separate test |
| Modbus RTU | COM ports; omit Linux-only `transport.rs485` direction control and use an automatic-direction adapter |
| EtherCAT | EtherCrab/Npcap native transport; `_sim` remains available; software checks passed, physical bus acceptance pending |
| CANopen | gs_usb/WinUSB native transport; software checks passed, physical bus acceptance pending; Linux SocketCAN is a separate backend |
| Linux edge deploy | Windows OpenSSH client and `tar.exe`; the runtime must match the Linux edge architecture |
| Windows runtime lifecycle | Foreground process; no Windows service installer, hard real-time claim or automatic restart guarantee |

Windows runtime `GET /system` enumerates native network interfaces and
COM ports. Carrier means connected media, not proof that a field device
is reachable. See [Windows CANopen and EtherCAT](windows-can-ethercat.md)
for external drivers, licensing and the separate bus acceptance record.
No physical CAN or EtherCAT device has been validated by this work.

## Install a prebuilt package

Extract `ia2-windows-x64.zip` into a writable directory. Open **Windows
PowerShell** there and run:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\scripts\install-skill.ps1
```

The package includes five binaries, built web assets, the FB library and
complete skill references/checklists. The release build uses a static
Microsoft C runtime, so installation needs no Rust, Node, pnpm or VC++
Redistributable installer. Physical EtherCAT additionally requires the
separately installed Npcap driver; that driver and its installer are not
included in the IA2 package. `windows-package.json` records the source
commit, dirty status, target and packaging time. The adjacent `.sha256`
file checks transfer integrity, not publisher identity; the ZIP is unsigned.

The desktop app requires **Microsoft Edge WebView2 Evergreen Runtime**.
The installer runs `IA2.exe --check-runtime` without opening a window or
starting a server, and checks its exit code before replacing an existing
installation. If WebView2 is missing, install it from
[Microsoft's WebView2 download page](https://developer.microsoft.com/microsoft-edge/webview2/)
and run the IA2 installer again. Use the Evergreen Standalone Installer
for an offline target. IA2 does not bundle or automatically install this
runtime, and does not change system proxy settings. Windows 11 normally
includes WebView2, but the app still checks availability; an installed
Edge browser alone is not the dependency check.

Default per-user layout:

```text
%LOCALAPPDATA%\IA2\
  IA2.ps1
  bin\IA2.exe
  bin\cs.exe
  bin\ia2-server.exe
  bin\lsp-launcher.exe
  bin\ia2-runtime.exe
  web\
  library\
%USERPROFILE%\.claude\skills\industrial-automation-skill\
%USERPROFILE%\.agents\skills\industrial-automation-skill\
```

The installer creates **IA2 IDE** in your Start menu and on your desktop;
both shortcuts point directly to `bin\IA2.exe`, without a console wrapper.
**IA2 Terminal** in the Start menu opens PowerShell for CLI work.
It needs no elevation or symlinks, does not change machine/user PATH or
shell profiles, and does not add firewall rules or start a service. IA2
Terminal puts `bin` on PATH only for that terminal and its child processes.
Start a coding agent there if it needs `cs` by name, or configure it to
use the absolute `cs.exe` path. Restart the agent to discover the skills.

Options: `-InstallRoot 'D:\Tools with spaces\IA2'` changes the application
directory; `-ClaudeDir` / `-AgentsDir` change agent roots; `-NoShortcuts`
omits both desktop and Start menu entries. `-SkillOnly` installs just the skills with no
build or network step.

Updates stage replacements before swapping each owned directory. The
installer refuses unmanaged destinations, junction/symlink paths, and
destinations overlapping sources or each other.
Exit installed IA2 through its tray menu before upgrading; merely closing
the window hides it. If a program is running, stop it explicitly first:
the installer does not stop programs or force running processes to exit.
Do not store projects in
application or skill directories: these contain replaceable shipped
assets. Normal projects remain in the Windows Documents known folder's
`IA2` directory, which may be redirected to OneDrive. Its
`.ia2-open-projects.json` and `%APPDATA%\IA2\state.toml` retain project
selection. Desktop browser data and logs live separately under
`%LOCALAPPDATA%\IA2Desktop`; upgrades preserve that directory too.
Custom project paths and a headless runtime's separate state directory
are also outside the installer. Removing the application, two installed
skills and desktop/Start menu shortcuts removes the shipped assets while
preserving this user data. The directory swaps are individually rolled
back on failure; installation is not a transaction across all directories.

## Start and stop

Double-click **IA2 IDE** on the desktop or in the Start menu. For a custom
port, or when using an extracted package directly:

```powershell
Start-Process "$env:LOCALAPPDATA\IA2\bin\IA2.exe"
# Custom port:
Start-Process "$env:LOCALAPPDATA\IA2\bin\IA2.exe" -ArgumentList '--port 3005'
```

Keep `bin`, `web` and `library` together; copying `IA2.exe` alone is not an
installation. The app starts its adjacent `ia2-server.exe`, passes the
installed web/library paths, and displays the local IDE after the server
is ready. It uses loopback, with port 3001 by default. Startup errors are
shown rather than treated as a successful launch. The existing
`IA2.ps1 -Port <port>` entry also starts the native app;
`IA2.ps1 -Terminal` opens only the CLI environment.

The standard Windows title bar, resize border, Snap and per-monitor DPI
remain native. The title bar and WebView background follow the workbench's
light/dark choice; the default is light regardless of the OS theme.
Startup and connection-error pages render locally without network assets.

**Closing the window hides IA2 to the notification-area tray; the server
and a running program continue.** Use the tray's Open command to return.
The tray's Exit command requests shutdown of the server owned by this
app. If a program is still running, exit is refused; stop the program
explicitly in the IDE or through the CLI and retry. The app does not kill
an active controller to make exit succeed. `IA2.exe --shutdown` uses the
same guarded exit path. Neither window close nor app exit is an emergency
stop, and this is not a Windows service.

In another PowerShell window:

```powershell
$cs = "$env:LOCALAPPDATA\IA2\bin\cs.exe"
& $cs api GET /health
& $cs ls projects
& $cs ls library
```

`LSP_LAUNCHER` overrides the language-server executable; by default the
server finds `lsp-launcher.exe` next to itself. The desktop server binds loopback.
For a custom port, pass `--server http://127.0.0.1:3005` to the CLI.
Project open accepts drive paths, UNC paths and `~\Documents\...`; quote
paths containing spaces. File names reject Windows reserved names and
invalid trailing dots/spaces so projects remain portable.

Library resolution order: `--library-dir`, `IA2_LIBRARY_DIR`, `./library`,
the executable's adjacent `../library`, then the legacy
`~/.local/share/ia2/library` location.

To run a Windows runtime development process, call `ia2-runtime.exe --help`
and launch it in a separate visible console with its documented project
arguments. Ctrl+C stops it. Linux service management and hardware timing
evidence do not transfer to this process.

## Build from source

Prerequisites: Git with the `vendor/ironplc` submodule, Rust's MSVC x64
toolchain, Visual Studio C++ Build Tools (Desktop development with C++ and
Windows SDK), and Node/pnpm matching root `package.json`. Use an x64 Native
Tools terminal if the linker or SDK cannot be found. GNU/MinGW is outside
this target. The native gate is validated with Rust **1.95.0**, matching
the existing macOS gate; newer Clippy releases can add warnings in the
locked upstream compiler. Select the validated toolchain for this clone:

```powershell
git clone --recursive https://github.com/supcon-international/ia2
Set-Location ia2
rustup toolchain install 1.95.0 --profile minimal --component clippy --component rustfmt
rustup override set 1.95.0
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\scripts\install-skill.ps1
```

The source installer initializes submodules, installs locked web
dependencies, runs server tests to generate TypeScript types, builds all
five release binaries and the web UI. Installation does not replace the
full quality gate. `-SkipBuild` consumes prebuilt
`target\x86_64-pc-windows-msvc\release` and `apps\web\dist` artifacts,
and fails if any required artifact is absent.

`scripts/build-windows.ps1` builds an explicit x64 MSVC release with
`-C target-feature=+crt-static`, preserving caller flags and restoring
them afterward. It leaves global Cargo config and ordinary debug builds
unchanged. Use this helper for distributable releases; a plain host
`cargo build --release` may depend on the VC++ runtime installed by the
development tools.

With Git symlinks disabled, `.agents\skills\industrial-automation-skill`
is a text placeholder, not a discoverable directory. Read canonical
`.claude\skills\industrial-automation-skill\SKILL.md` directly for repo
work, or use `-SkillOnly` to install real user copies. Git symlinks need
Developer Mode or appropriate privileges; the installer needs neither.

## Native acceptance and packaging

```powershell
pnpm install --frozen-lockfile
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\scripts\check-windows.ps1
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\scripts\package-windows.ps1 -SkipBuild
```

The gate checks skill metadata/discovery and installation (upgrades,
spaces in paths and refusal guards), formatting, workspace Clippy with
zero warnings, workspace tests, five release binaries and web build/tests.
It builds `server.exe` before CLI simulation tests: missing executables
are failures, not successfully skipped business tests. `-AdaptationOnly`
runs just the offline installer checks. Linux Bash deployment execution
tests still need Unix coverage; Windows exercises real local archive
operations and script generation. The gate also runs
`scripts/test-windows-runtime.ps1` against the static-CRT release: a
device-free scenario in a Chinese path with spaces, real system inventory,
pause/write/step/resume, and confirmed clean process stop. Its JSON/log
evidence stays under `target/windows-runtime-smoke/`. That script may be
run separately with `-RuntimePath` to select an executable.

Without `-SkipBuild`, `package-windows.ps1` also builds. It verifies all
five binaries have x64 PE headers, checks their static-CRT/Npcap import
boundaries, and uses .NET ZIP creation to include
the hidden `.claude` tree. `-OutputPath` accepts paths containing spaces.
Default output: `dist\ia2-windows-x64.zip` and its SHA256 file.

Verify the extracted release separately from the source gate:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\scripts\test-windows-install.ps1 -PackagePath .\dist\ia2-windows-x64.zip
```

This checks the ZIP hash and hidden skill files, two identical installations
in isolated Chinese paths with spaces, unchanged user data/PATH, installed
CLI/runtime execution, the desktop WebView2 prerequisite check, server
startup outside the repository, bundled web
assets, automatic library discovery and a real LSP initialize exchange.
It runs with only Windows system directories on PATH, stops its server and
sidecar, and saves results/logs under `target/windows-install-smoke/`.
The test does not install into the default user location, create shortcuts
or count the prerequisite check as native-window acceptance.

Before distribution, extract the ZIP into a different path containing
spaces, install it, and verify a real project create/open, compilation,
language-server connection, scenario, monitor updates, library import,
HMI load and live data in the native window. Check tray hide/reopen,
refusal to exit while a program runs, then confirmed process shutdown
after the program stops. Launch again through the desktop/Start menu
shortcut and check data persistence and frontend errors. Source tests do
not replace installed-artifact checks. Check light/dark theme changes,
including light IA2 on a dark Windows desktop, maximization/restore/Snap,
and supported monitor scaling: there should be no exposed dark strip or
unpainted client area. Report
physical serial/network integration and Linux edge deployment with their
own evidence.

## Deploy from Windows to Linux

Windows is the engineering host; `cs deploy` still provisions Linux using
SSH, Bash and systemd. Install Windows OpenSSH Client if `ssh.exe` is absent,
and ensure `tar.exe` is on the server's PATH. Put edge aliases in
`%USERPROFILE%\.ssh\config` and verify key-based
`ssh -o BatchMode=yes <alias>` first.

Installed `ia2-runtime.exe` is never a Linux payload. Supply a Linux ELF
matching the target architecture through `IA2_RUNTIME_BIN`, or reuse the
runtime already provisioned there. See [edge deployment](edge-deploy.md)
for Linux provisioning and failure reporting. Local simulation alone is
not hardware readiness evidence.

## Implementation references

- [Microsoft: Rust prerequisites on Windows](https://learn.microsoft.com/en-us/windows/dev-environment/rust/setup)
- [Microsoft: WebView2 Runtime deployment and detection](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/distribution)
- [Rust: static and dynamic C runtimes](https://doc.rust-lang.org/reference/linkage.html#static-and-dynamic-c-runtimes)
- [Microsoft: Windows OpenSSH setup](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_install_firstuse)
- [Microsoft: Start-Process quoting](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.management/start-process?view=powershell-5.1)
- [Microsoft: Compress-Archive omits hidden files](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.archive/compress-archive?view=powershell-5.1)
