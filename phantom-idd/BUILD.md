# Build Instructions

## Prerequisites

1. **Visual Studio 2022** with "Desktop development with C++" workload
2. **Windows Driver Kit (WDK)** — download from https://learn.microsoft.com/en-us/windows-hardware/drivers/download-the-wdk

## Build Steps

### Option A: Command Line (MSBuild)

```cmd
:: Open "Developer Command Prompt for VS 2022"
cd phantom-idd
msbuild PhantomIDD.vcxproj /p:Configuration=Release /p:Platform=x64
```

Output: `x64\Release\PhantomIDD\PhantomIDD.dll`

### Option B: Visual Studio

1. Open `PhantomIDD.vcxproj` in VS 2022
2. Set configuration to Release/x64
3. Build

## Test on VM

```cmd
:: Copy files to VM
scp x64\Release\PhantomIDD\PhantomIDD.dll user@vm:C:\PhantomIDD\
scp PhantomIDD.inf user@vm:C:\PhantomIDD\

:: On VM: enable test signing (one-time)
bcdedit /set testsigning on
shutdown /r /t 0

:: After reboot: install
nefconw install C:\PhantomIDD\PhantomIDD.inf Root\PhantomIDD
```

## CI/CD (GitHub Actions)

```yaml
- name: Build IDD Driver
  run: |
    msbuild phantom-idd/PhantomIDD.vcxproj /p:Configuration=Release /p:Platform=x64
  shell: cmd
```

Note: GitHub Actions windows-latest has VS 2022 but may not have WDK.
Install WDK in CI: `choco install windowsdriverkit11 -y`
