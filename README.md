# victus-control

A tiny Windows tray app that switches the performance mode and max fan on an
HP Victus 16 laptop, so OMEN Gaming Hub isn't needed for that. It talks to the
HP BIOS directly through WMI, the same interface Gaming Hub uses.

- **Performance mode:** Quiet, Default or Performance
- **Max fan:** on, or back to AUTO
- Remembers your choice and puts it back at logon, on plug-in and on wake
- One ~425 KB standalone exe, no installer, no services; about 1.6 MB of RAM and no CPU while idle

The tray icon shows the current mode: a snail for Quiet, a rabbit for Default
and a leopard for Performance.

## Supported hardware

Only tested on the **HP Victus 16-d1xxx (board ID 8A25)**. On any other board
the app reads the state but refuses to install or change anything (pass
`--force` with CLI/tray commands to override; `--install` refuses unsupported
boards). Other HP Victus and OMEN models may use the same BIOS interface with
different values; `--force` on an unverified board is at your own risk.

You can see your board ID with:

```powershell
(Get-ItemProperty HKLM:\HARDWARE\DESCRIPTION\System\BIOS).BaseBoardProduct
```

## Requirements

- Windows 11, 24H2 or later (older versions work, but a console window opens
  alongside the tray)
- Administrator rights: the HP BIOS interface refuses even read requests from
  non-admin processes. The exe asks for elevation when started by hand; the
  logon task starts it elevated without a prompt.

## Install

Build it (see [Building](#building)), then from an elevated terminal:

```powershell
.\victus-control.exe --install
```

This copies the exe to `C:\Program Files\victus-control\`, creates a Task
Scheduler task `victus-control` that starts it at your logon with highest
privileges, and starts the tray. The copy lives in Program Files because only
administrators can write there: the task starts it elevated, so nothing else may
be able to replace it.

To remove it again:

```powershell
& "C:\Program Files\victus-control\victus-control.exe" --uninstall
```

This closes the tray and deletes the task and the Program Files folder,
including the saved setting and the log (see [Files](#files)).

## Tray menu

Click or right-click the icon:

```
Mode: Performance · Fans 2191 / 2387 rpm
---------------------------------
( ) Quiet
( ) Default
(•) Performance
---------------------------------
[ ] Max fan
---------------------------------
[x] Start with Windows
Exit
```

The status line and checkmarks are read from the BIOS each time the menu opens.
**Start with Windows** turns the logon task off and on; it's only available
when the tray runs from the Program Files copy.

## Command line

The same exe works as a command-line tool (from an elevated terminal):

```
victus-control status                           show mode, max fan, fan speeds, GPU power
victus-control mode quiet|default|performance   set the performance mode
victus-control fan max|auto                     set max fan on / back to AUTO
victus-control --install                        copy to Program Files and start at logon
victus-control --uninstall                      remove the logon task and Program Files copy
```

Mode and fan changes made here are remembered too, just like menu choices.

## How it behaves

- **On battery** the BIOS switches to Default and ignores requests for
  Performance. The menu shows Performance greyed out until you plug in again;
  then the remembered mode is put back.
- **After sleep** the BIOS may fall back to Default. When the display comes
  back on, the tray checks and re-applies the remembered setting.
- **At logon** the tray does the same check once.
- **Alongside Gaming Hub:** Gaming Hub sometimes switches the mode on its own,
  for example right after plugging in. The tray puts your mode back, but it's
  meant to replace Gaming Hub rather than run next to it.

## Files

| What | Where |
| --- | --- |
| Remembered setting | `C:\Program Files\victus-control\config.toml` |
| Log of what was set and why | `C:\Program Files\victus-control\log.txt` (rotated at 256 KB) |

Both live next to the exe, where only administrators can write. The app runs
as administrator, so it never writes to a folder that other programs could
tamper with.

`config.toml` is plain text and can be edited by hand, from an editor started
as administrator:

```toml
mode = "performance"
max_fan = false
```

## Building

Needs Rust (edition 2024) with the MSVC toolchain.

```powershell
cargo build --release
```

The exe ends up in `target\release\victus-control.exe`. Since it embeds a
manifest that requires administrator rights, `cargo test` and `cargo run` also
need an elevated terminal.

## License

MIT, see [LICENSE](LICENSE).

The tray icons are [Fluent Emoji](https://github.com/microsoft/fluentui-emoji)
by Microsoft, also MIT licensed; see
[assets/fluentui-emoji-LICENSE.txt](assets/fluentui-emoji-LICENSE.txt).

Not affiliated with or endorsed by HP. This uses an undocumented BIOS
interface; use it at your own risk.
