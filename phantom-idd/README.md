# Phantom IDD — Indirect Display Driver

Minimal IDD (Indirect Display Driver) for Phantom remote desktop.
Creates a virtual display with **dynamic resolution** support via named pipe IPC.

## Why not MiketheTech VDD?

| Feature | MiketheTech VDD | Phantom IDD |
|---------|----------------|-------------|
| Dynamic resolution | No (static XML, needs device restart) | Yes (`IddCxMonitorUpdateModes`) |
| IPC | None | Named pipe (`\\.\pipe\PhantomIDD`) |
| GPU targeting | friendlyname in XML (unreliable) | `IddCxAdapterSetRenderAdapter` with LUID |
| Arbitrary resolution | No (only preset list) | Yes (any width x height) |

Same approach as DCV (AWS Indirect Display Device) and RustDesk IDD.

## Build

Requires:
- Visual Studio 2022
- Windows Driver Kit (WDK)

```cmd
msbuild PhantomIDD.vcxproj /p:Configuration=Release /p:Platform=x64
```

## Install

```cmd
:: Enable test signing (one-time, needs reboot)
bcdedit /set testsigning on

:: Install driver
nefconw install PhantomIDD.inf Root\PhantomIDD
```

## IPC Protocol

Connect to `\\.\pipe\PhantomIDD` and write 8 bytes:

```
[u32 little-endian: width][u32 little-endian: height]
```

The driver will call `IddCxMonitorUpdateModes` to inject the new resolution
immediately. No device restart needed.

## Integration with phantom-server

`phantom-server --install` will:
1. Copy PhantomIDD.dll + PhantomIDD.inf to install dir
2. Install via nefconw
3. At runtime, connect to the named pipe to set resolution matching client viewport
