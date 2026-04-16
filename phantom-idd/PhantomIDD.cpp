// Phantom IDD — Indirect Display Driver for headless GPU servers.
// Creates a virtual display with dynamic resolution support via named pipe IPC.
// Based on Microsoft IddSampleDriver (MIT), stripped to essentials.
//
// Build: Visual Studio + WDK (Windows Driver Kit)
// Install: nefconw install PhantomIDD.inf Root\PhantomIDD
// IPC: \\.\pipe\PhantomIDD — send [u32 width][u32 height] to change resolution

#include <windows.h>
#include <wdf.h>
#include <iddcx.h>
#include <avrt.h>
#include <wrl.h>
#include <vector>
#include <mutex>
#include <thread>

using namespace Microsoft::WRL;

// ── Constants ──────────────────────────────────────────────────────────────

static constexpr DWORD DEFAULT_WIDTH = 1920;
static constexpr DWORD DEFAULT_HEIGHT = 1080;
static constexpr DWORD DEFAULT_VREFRESH = 60;
static constexpr PCWSTR PIPE_NAME = L"\\\\.\\pipe\\PhantomIDD";

// ── Forward declarations ───────────────────────────────────────────────────

EVT_WDF_DRIVER_DEVICE_ADD              PhantomDeviceAdd;
EVT_WDF_DEVICE_D0_ENTRY               PhantomDeviceD0Entry;
EVT_IDD_CX_ADAPTER_INIT_FINISHED      PhantomAdapterInitFinished;
EVT_IDD_CX_ADAPTER_COMMIT_MODES       PhantomAdapterCommitModes;
EVT_IDD_CX_PARSE_MONITOR_DESCRIPTION  PhantomParseMonitorDescription;
EVT_IDD_CX_MONITOR_GET_DEFAULT_DESCRIPTION_MODES PhantomMonitorGetDefaultModes;
EVT_IDD_CX_MONITOR_QUERY_TARGET_MODES PhantomMonitorQueryTargetModes;
EVT_IDD_CX_MONITOR_ASSIGN_SWAPCHAIN   PhantomMonitorAssignSwapChain;
EVT_IDD_CX_MONITOR_UNASSIGN_SWAPCHAIN PhantomMonitorUnassignSwapChain;

// ── Helpers ────────────────────────────────────────────────────────────────

static IDDCX_MONITOR_MODE CreateMonitorMode(DWORD w, DWORD h, DWORD vrefresh)
{
    IDDCX_MONITOR_MODE mode = {};
    mode.Size = sizeof(mode);
    mode.Origin = IDDCX_MONITOR_MODE_ORIGIN_DRIVER;
    mode.MonitorVideoSignalInfo.totalSize.cx = w;
    mode.MonitorVideoSignalInfo.totalSize.cy = h;
    mode.MonitorVideoSignalInfo.activeSize.cx = w;
    mode.MonitorVideoSignalInfo.activeSize.cy = h;
    mode.MonitorVideoSignalInfo.AdditionalSignalInfo.vSyncFreqDivider = 1;
    mode.MonitorVideoSignalInfo.AdditionalSignalInfo.videoStandard = 255;
    mode.MonitorVideoSignalInfo.vSyncFreq.Numerator = vrefresh;
    mode.MonitorVideoSignalInfo.vSyncFreq.Denominator = 1;
    mode.MonitorVideoSignalInfo.hSyncFreq.Numerator = vrefresh * h;
    mode.MonitorVideoSignalInfo.hSyncFreq.Denominator = 1;
    mode.MonitorVideoSignalInfo.scanLineOrdering = DISPLAYCONFIG_SCANLINE_ORDERING_PROGRESSIVE;
    mode.MonitorVideoSignalInfo.pixelRate = (UINT64)w * h * vrefresh;
    return mode;
}

static IDDCX_TARGET_MODE CreateTargetMode(DWORD w, DWORD h, DWORD vrefresh)
{
    IDDCX_TARGET_MODE mode = {};
    mode.Size = sizeof(mode);
    mode.TargetVideoSignalInfo.totalSize.cx = w;
    mode.TargetVideoSignalInfo.totalSize.cy = h;
    mode.TargetVideoSignalInfo.activeSize.cx = w;
    mode.TargetVideoSignalInfo.activeSize.cy = h;
    mode.TargetVideoSignalInfo.AdditionalSignalInfo.vSyncFreqDivider = 1;
    mode.TargetVideoSignalInfo.AdditionalSignalInfo.videoStandard = 255;
    mode.TargetVideoSignalInfo.vSyncFreq.Numerator = vrefresh;
    mode.TargetVideoSignalInfo.vSyncFreq.Denominator = 1;
    mode.TargetVideoSignalInfo.hSyncFreq.Numerator = vrefresh * h;
    mode.TargetVideoSignalInfo.hSyncFreq.Denominator = 1;
    mode.TargetVideoSignalInfo.scanLineOrdering = DISPLAYCONFIG_SCANLINE_ORDERING_PROGRESSIVE;
    mode.TargetVideoSignalInfo.targetVideoSignalPixelRate = (UINT64)w * h * vrefresh;
    return mode;
}

// ── Device context ─────────────────────────────────────────────────────────

struct PhantomDeviceContext
{
    IDDCX_ADAPTER adapter = nullptr;
    IDDCX_MONITOR monitor = nullptr;

    // Current resolution (updated via IPC)
    std::mutex modeLock;
    DWORD currentWidth = DEFAULT_WIDTH;
    DWORD currentHeight = DEFAULT_HEIGHT;

    // IPC thread
    std::thread pipeThread;
    bool stopping = false;

    void InitAdapter();
    void CreateMonitor();
    void StartPipeServer();
    void UpdateResolution(DWORD w, DWORD h);
};

WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(PhantomDeviceContext, GetDeviceContext);

// ── IPC: Named pipe server for resolution changes ──────────────────────────

void PhantomDeviceContext::StartPipeServer()
{
    pipeThread = std::thread([this]()
    {
        while (!stopping)
        {
            HANDLE pipe = CreateNamedPipeW(
                PIPE_NAME,
                PIPE_ACCESS_INBOUND,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,      // max instances
                0,      // out buffer
                8,      // in buffer (u32 width + u32 height)
                1000,   // timeout ms
                nullptr);

            if (pipe == INVALID_HANDLE_VALUE) {
                Sleep(1000);
                continue;
            }

            if (ConnectNamedPipe(pipe, nullptr) || GetLastError() == ERROR_PIPE_CONNECTED)
            {
                while (!stopping)
                {
                    BYTE buf[8];
                    DWORD bytesRead = 0;
                    if (!ReadFile(pipe, buf, 8, &bytesRead, nullptr) || bytesRead < 8)
                        break;

                    DWORD w = *(DWORD*)&buf[0];
                    DWORD h = *(DWORD*)&buf[4];

                    if (w >= 640 && w <= 7680 && h >= 480 && h <= 4320)
                    {
                        // Round to even (H.264 requirement)
                        w &= ~1u;
                        h &= ~1u;
                        UpdateResolution(w, h);
                    }
                }
            }

            DisconnectNamedPipe(pipe);
            CloseHandle(pipe);
        }
    });
}

void PhantomDeviceContext::UpdateResolution(DWORD w, DWORD h)
{
    {
        std::lock_guard<std::mutex> lock(modeLock);
        if (w == currentWidth && h == currentHeight)
            return;
        currentWidth = w;
        currentHeight = h;
    }

    if (monitor == nullptr)
        return;

    // Dynamically inject the new resolution — this is the key advantage
    // over MiketheTech VDD (which only supports static XML config).
    // DCV and RustDesk use the same IddCxMonitorUpdateModes approach.
    IDDCX_MONITOR_MODE newMode = CreateMonitorMode(w, h, DEFAULT_VREFRESH);

    IDARG_IN_MONITORUPDATEMODES args = {};
    args.MonitorModeCount = 1;
    args.pMonitorModes = &newMode;

    IddCxMonitorUpdateModes(monitor, &args);
}

// ── Adapter + Monitor lifecycle ────────────────────────────────────────────

void PhantomDeviceContext::InitAdapter()
{
    IDDCX_ADAPTER_CAPS caps = {};
    caps.Size = sizeof(caps);
    caps.MaxMonitorsSupported = 1;

    caps.EndPointDiagnostics.Size = sizeof(caps.EndPointDiagnostics);
    caps.EndPointDiagnostics.GammaSupport = IDDCX_FEATURE_IMPLEMENTATION_NONE;
    caps.EndPointDiagnostics.TransmissionType = IDDCX_TRANSMISSION_TYPE_WIRED_OTHER;
    caps.EndPointDiagnostics.pEndPointFriendlyName = L"Phantom Virtual Display";
    caps.EndPointDiagnostics.pEndPointManufacturerName = L"Phantom";
    caps.EndPointDiagnostics.pEndPointModelName = L"PhantomIDD";

    IDDCX_ENDPOINT_VERSION ver = {};
    ver.Size = sizeof(ver);
    ver.MajorVer = 1;
    caps.EndPointDiagnostics.pFirmwareVersion = &ver;
    caps.EndPointDiagnostics.pHardwareVersion = &ver;

    IDARG_IN_ADAPTER_INIT initArgs = {};
    initArgs.WdfDevice = WdfObjectContextGetObject(this);
    initArgs.pCaps = &caps;
    initArgs.ObjectAttributes.Size = sizeof(initArgs.ObjectAttributes);

    IDARG_OUT_ADAPTER_INIT initOut;
    IddCxAdapterInitAsync(&initArgs, &initOut);
    adapter = initOut.AdapterObject;
}

void PhantomDeviceContext::CreateMonitor()
{
    // No EDID — use GetDefaultDescriptionModes for mode reporting.
    // This is the simplest path for a virtual display.
    IDDCX_MONITOR_INFO monInfo = {};
    monInfo.Size = sizeof(monInfo);
    monInfo.MonitorType = DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI;
    monInfo.ConnectorIndex = 0;
    monInfo.MonitorDescription.Size = sizeof(monInfo.MonitorDescription);
    monInfo.MonitorDescription.Type = IDDCX_MONITOR_DESCRIPTION_TYPE_EDID;
    monInfo.MonitorDescription.DataSize = 0;
    monInfo.MonitorDescription.pData = nullptr;

    CoCreateGuid(&monInfo.MonitorContainerId);

    IDARG_IN_MONITORCREATE createArgs = {};
    createArgs.pMonitorInfo = &monInfo;

    WDF_OBJECT_ATTRIBUTES attr;
    WDF_OBJECT_ATTRIBUTES_INIT(&attr);
    createArgs.ObjectAttributes = &attr;

    IDARG_OUT_MONITORCREATE createOut;
    NTSTATUS status = IddCxMonitorCreate(adapter, &createArgs, &createOut);
    if (!NT_SUCCESS(status))
        return;

    monitor = createOut.MonitorObject;

    // Announce monitor arrival — Windows will now query for modes
    IDARG_OUT_MONITORARRIVAL arrivalOut;
    IddCxMonitorArrival(monitor, &arrivalOut);

    // Try to set render adapter to NVIDIA GPU (for DXGI zero-copy)
#if IDD_IS_FUNCTION_AVAILABLE(IddCxAdapterSetRenderAdapter)
    {
        // Enumerate DXGI adapters, find NVIDIA
        IDXGIFactory1* factory = nullptr;
        if (SUCCEEDED(CreateDXGIFactory1(IID_PPV_ARGS(&factory))))
        {
            IDXGIAdapter1* dxgiAdapter = nullptr;
            for (UINT i = 0; factory->EnumAdapters1(i, &dxgiAdapter) != DXGI_ERROR_NOT_FOUND; i++)
            {
                DXGI_ADAPTER_DESC1 desc;
                dxgiAdapter->GetDesc1(&desc);
                // Check for NVIDIA vendor ID (0x10DE)
                if (desc.VendorId == 0x10DE)
                {
                    IDARG_IN_ADAPTERSETRENDERADAPTER renderArgs = {};
                    renderArgs.PreferredRenderAdapter = desc.AdapterLuid;
                    IddCxAdapterSetRenderAdapter(adapter, &renderArgs);
                    dxgiAdapter->Release();
                    break;
                }
                dxgiAdapter->Release();
            }
            factory->Release();
        }
    }
#endif

    // Start IPC pipe server for resolution changes
    StartPipeServer();
}

// ── WDF + IddCx callbacks ─────────────────────────────────────────────────

extern "C" NTSTATUS DriverEntry(PDRIVER_OBJECT pDriverObject, PUNICODE_STRING pRegistryPath)
{
    WDF_DRIVER_CONFIG config;
    WDF_OBJECT_ATTRIBUTES attrs;
    WDF_OBJECT_ATTRIBUTES_INIT(&attrs);
    WDF_DRIVER_CONFIG_INIT(&config, PhantomDeviceAdd);
    return WdfDriverCreate(pDriverObject, pRegistryPath, &attrs, &config, WDF_NO_HANDLE);
}

NTSTATUS PhantomDeviceAdd(WDFDRIVER Driver, PWDFDEVICE_INIT pDeviceInit)
{
    UNREFERENCED_PARAMETER(Driver);

    // PnP power callbacks
    WDF_PNPPOWER_EVENT_CALLBACKS pnp;
    WDF_PNPPOWER_EVENT_CALLBACKS_INIT(&pnp);
    pnp.EvtDeviceD0Entry = PhantomDeviceD0Entry;
    WdfDeviceInitSetPnpPowerEventCallbacks(pDeviceInit, &pnp);

    // IddCx callbacks
    IDD_CX_CLIENT_CONFIG iddConfig;
    IDD_CX_CLIENT_CONFIG_INIT(&iddConfig);
    iddConfig.EvtIddCxAdapterInitFinished = PhantomAdapterInitFinished;
    iddConfig.EvtIddCxAdapterCommitModes = PhantomAdapterCommitModes;
    iddConfig.EvtIddCxParseMonitorDescription = PhantomParseMonitorDescription;
    iddConfig.EvtIddCxMonitorGetDefaultDescriptionModes = PhantomMonitorGetDefaultModes;
    iddConfig.EvtIddCxMonitorQueryTargetModes = PhantomMonitorQueryTargetModes;
    iddConfig.EvtIddCxMonitorAssignSwapChain = PhantomMonitorAssignSwapChain;
    iddConfig.EvtIddCxMonitorUnassignSwapChain = PhantomMonitorUnassignSwapChain;

    NTSTATUS status = IddCxDeviceInitConfig(pDeviceInit, &iddConfig);
    if (!NT_SUCCESS(status))
        return status;

    // Create device with context
    WDF_OBJECT_ATTRIBUTES deviceAttrs;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&deviceAttrs, PhantomDeviceContext);
    deviceAttrs.EvtCleanupCallback = [](WDFOBJECT obj) {
        auto* ctx = GetDeviceContext(obj);
        ctx->stopping = true;
        if (ctx->pipeThread.joinable())
            ctx->pipeThread.join();
    };

    WDFDEVICE device = nullptr;
    status = WdfDeviceCreate(&pDeviceInit, &deviceAttrs, &device);
    if (!NT_SUCCESS(status))
        return status;

    status = IddCxDeviceInitialize(device);
    return status;
}

NTSTATUS PhantomDeviceD0Entry(WDFDEVICE Device, WDF_POWER_DEVICE_STATE PreviousState)
{
    UNREFERENCED_PARAMETER(PreviousState);
    auto* ctx = GetDeviceContext(Device);
    ctx->InitAdapter();
    return STATUS_SUCCESS;
}

NTSTATUS PhantomAdapterInitFinished(IDDCX_ADAPTER Adapter, const IDARG_IN_ADAPTER_INIT_FINISHED* pInArgs)
{
    auto* ctx = GetDeviceContext(Adapter);
    if (NT_SUCCESS(pInArgs->AdapterInitStatus))
    {
        ctx->CreateMonitor();
    }
    return STATUS_SUCCESS;
}

NTSTATUS PhantomAdapterCommitModes(IDDCX_ADAPTER Adapter, const IDARG_IN_COMMITMODES* pInArgs)
{
    UNREFERENCED_PARAMETER(Adapter);
    UNREFERENCED_PARAMETER(pInArgs);
    return STATUS_SUCCESS;
}

NTSTATUS PhantomParseMonitorDescription(const IDARG_IN_PARSEMONITORDESCRIPTION* pInArgs,
                                         IDARG_OUT_PARSEMONITORDESCRIPTION* pOutArgs)
{
    // We use edid-less monitor, so this shouldn't be called.
    // Return empty to be safe.
    pOutArgs->MonitorModeBufferOutputCount = 0;
    UNREFERENCED_PARAMETER(pInArgs);
    return STATUS_SUCCESS;
}

NTSTATUS PhantomMonitorGetDefaultModes(IDDCX_MONITOR Monitor,
    const IDARG_IN_GETDEFAULTDESCRIPTIONMODES* pInArgs,
    IDARG_OUT_GETDEFAULTDESCRIPTIONMODES* pOutArgs)
{
    auto* ctx = GetDeviceContext(Monitor);
    std::lock_guard<std::mutex> lock(ctx->modeLock);

    // Report current resolution as the single supported mode
    if (pInArgs->DefaultMonitorModeBufferInputCount == 0)
    {
        pOutArgs->DefaultMonitorModeBufferOutputCount = 1;
    }
    else
    {
        pInArgs->pDefaultMonitorModes[0] = CreateMonitorMode(
            ctx->currentWidth, ctx->currentHeight, DEFAULT_VREFRESH);
        pOutArgs->DefaultMonitorModeBufferOutputCount = 1;
        pOutArgs->PreferredMonitorModeIdx = 0;
    }
    return STATUS_SUCCESS;
}

NTSTATUS PhantomMonitorQueryTargetModes(IDDCX_MONITOR Monitor,
    const IDARG_IN_QUERYTARGETMODES* pInArgs,
    IDARG_OUT_QUERYTARGETMODES* pOutArgs)
{
    auto* ctx = GetDeviceContext(Monitor);
    std::lock_guard<std::mutex> lock(ctx->modeLock);

    // Report current resolution as target mode.
    // With IddCxMonitorUpdateModes, this gets refreshed dynamically.
    IDDCX_TARGET_MODE mode = CreateTargetMode(
        ctx->currentWidth, ctx->currentHeight, DEFAULT_VREFRESH);

    pOutArgs->TargetModeBufferOutputCount = 1;
    if (pInArgs->TargetModeBufferInputCount >= 1)
    {
        pInArgs->pTargetModes[0] = mode;
    }
    return STATUS_SUCCESS;
}

NTSTATUS PhantomMonitorAssignSwapChain(IDDCX_MONITOR Monitor,
    const IDARG_IN_SETSWAPCHAIN* pInArgs)
{
    // We don't process the swapchain — DXGI Desktop Duplication captures
    // from the display output, not from our swapchain processing.
    UNREFERENCED_PARAMETER(Monitor);
    UNREFERENCED_PARAMETER(pInArgs);
    return STATUS_SUCCESS;
}

NTSTATUS PhantomMonitorUnassignSwapChain(IDDCX_MONITOR Monitor)
{
    UNREFERENCED_PARAMETER(Monitor);
    return STATUS_SUCCESS;
}
