// Phantom IDD — Indirect Display Driver for headless GPU servers.
// Creates a virtual display with dynamic resolution support via named pipe IPC.
// Based on Microsoft IddSampleDriver (MIT), stripped to essentials.
//
// Build: Visual Studio + WDK (Windows Driver Kit)
// Install: nefconw install PhantomIDD.inf Root\PhantomIDD
// IPC: \\.\pipe\PhantomIDD — send [u32 width][u32 height] to change resolution

#include <windows.h>
#include <bugcodes.h>
#include <wudfwdm.h>
#include <wdf.h>
#include <IddCx.h>
#include <dxgi.h>
#include <wrl.h>
#include <vector>
#include <mutex>
#include <thread>

using namespace Microsoft::WRL;

// ── Debug logging ──────────────────────────────────────────────────────────

static void IddLog(const char* msg)
{
    HANDLE f = CreateFileW(L"C:\\Windows\\Temp\\phantom-idd.log",
        FILE_APPEND_DATA, FILE_SHARE_READ | FILE_SHARE_WRITE,
        nullptr, OPEN_ALWAYS, FILE_ATTRIBUTE_NORMAL, nullptr);
    if (f != INVALID_HANDLE_VALUE)
    {
        DWORD written;
        WriteFile(f, msg, (DWORD)strlen(msg), &written, nullptr);
        WriteFile(f, "\r\n", 2, &written, nullptr);
        CloseHandle(f);
    }
}

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

static void FillSignalInfo(DISPLAYCONFIG_VIDEO_SIGNAL_INFO& info, DWORD w, DWORD h, DWORD vrefresh)
{
    info.totalSize.cx = w;
    info.totalSize.cy = h;
    info.activeSize.cx = w;
    info.activeSize.cy = h;
    info.AdditionalSignalInfo.vSyncFreqDivider = 1;
    info.AdditionalSignalInfo.videoStandard = 255;
    info.vSyncFreq.Numerator = vrefresh;
    info.vSyncFreq.Denominator = 1;
    info.hSyncFreq.Numerator = vrefresh * h;
    info.hSyncFreq.Denominator = 1;
    info.scanLineOrdering = DISPLAYCONFIG_SCANLINE_ORDERING_PROGRESSIVE;
    info.pixelRate = (UINT64)w * h * vrefresh;
}

static IDDCX_MONITOR_MODE CreateMonitorMode(DWORD w, DWORD h, DWORD vrefresh)
{
    IDDCX_MONITOR_MODE mode = {};
    mode.Size = sizeof(mode);
    mode.Origin = IDDCX_MONITOR_MODE_ORIGIN_DRIVER;
    FillSignalInfo(mode.MonitorVideoSignalInfo, w, h, vrefresh);
    return mode;
}

static IDDCX_TARGET_MODE CreateTargetMode(DWORD w, DWORD h, DWORD vrefresh)
{
    IDDCX_TARGET_MODE mode = {};
    mode.Size = sizeof(mode);
    FillSignalInfo(mode.TargetVideoSignalInfo.targetVideoSignalInfo, w, h, vrefresh);
    return mode;
}

// ── Device context ─────────────────────────────────────────────────────────

struct PhantomDeviceContext
{
    WDFDEVICE wdfDevice = nullptr;
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

// Static context pointer (safe for single-monitor driver).
static PhantomDeviceContext* g_ctx = nullptr;

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

    // Dynamically update modes — the key advantage over MiketheTech VDD.
    if (IDD_IS_FUNCTION_AVAILABLE(IddCxMonitorUpdateModes))
    {
        IDDCX_TARGET_MODE targetMode = CreateTargetMode(w, h, DEFAULT_VREFRESH);

        IDARG_IN_UPDATEMODES args = {};
        args.Reason = IDDCX_UPDATE_REASON_OTHER;
        args.TargetModeCount = 1;
        args.pTargetModes = &targetMode;

        IddCxMonitorUpdateModes(monitor, &args);
    }
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
    initArgs.WdfDevice = wdfDevice;
    initArgs.pCaps = &caps;
    // ObjectAttributes left as default (nullptr)

    IddLog("InitAdapter: calling IddCxAdapterInitAsync");
    IDARG_OUT_ADAPTER_INIT initOut;
    NTSTATUS status = IddCxAdapterInitAsync(&initArgs, &initOut);
    char buf[64]; sprintf_s(buf, "IddCxAdapterInitAsync: 0x%08X", status); IddLog(buf);
    if (NT_SUCCESS(status))
    {
        adapter = initOut.AdapterObject;
    }
}

void PhantomDeviceContext::CreateMonitor()
{
    // No EDID — use GetDefaultDescriptionModes for mode reporting.
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

    IddLog("CreateMonitor: calling IddCxMonitorCreate");
    IDARG_OUT_MONITORCREATE createOut;
    NTSTATUS status = IddCxMonitorCreate(adapter, &createArgs, &createOut);
    {char buf[64]; sprintf_s(buf, "IddCxMonitorCreate: 0x%08X", status); IddLog(buf);}
    if (!NT_SUCCESS(status))
        return;

    monitor = createOut.MonitorObject;

    IddLog("CreateMonitor: calling IddCxMonitorArrival");
    IDARG_OUT_MONITORARRIVAL arrivalOut;
    status = IddCxMonitorArrival(monitor, &arrivalOut);
    {char buf[64]; sprintf_s(buf, "IddCxMonitorArrival: 0x%08X", status); IddLog(buf);}

    // Try to set render adapter to NVIDIA GPU (for DXGI zero-copy)
#if IDD_IS_FUNCTION_AVAILABLE(IddCxAdapterSetRenderAdapter)
    {
        IDXGIFactory1* factory = nullptr;
        if (SUCCEEDED(CreateDXGIFactory1(IID_PPV_ARGS(&factory))))
        {
            IDXGIAdapter1* dxgiAdapter = nullptr;
            for (UINT i = 0; factory->EnumAdapters1(i, &dxgiAdapter) != DXGI_ERROR_NOT_FOUND; i++)
            {
                DXGI_ADAPTER_DESC1 desc;
                dxgiAdapter->GetDesc1(&desc);
                if (desc.VendorId == 0x10DE) // NVIDIA
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

    StartPipeServer();
}

// ── WDF + IddCx callbacks ─────────────────────────────────────────────────

extern "C" NTSTATUS DriverEntry(PDRIVER_OBJECT pDriverObject, PUNICODE_STRING pRegistryPath)
{
    IddLog("DriverEntry called");
    WDF_DRIVER_CONFIG config;
    WDF_OBJECT_ATTRIBUTES attrs;
    WDF_OBJECT_ATTRIBUTES_INIT(&attrs);
    WDF_DRIVER_CONFIG_INIT(&config, PhantomDeviceAdd);
    NTSTATUS s = WdfDriverCreate(pDriverObject, pRegistryPath, &attrs, &config, WDF_NO_HANDLE);
    char buf[64]; sprintf_s(buf, "DriverEntry result: 0x%08X", s); IddLog(buf);
    return s;
}

NTSTATUS PhantomDeviceAdd(WDFDRIVER Driver, PWDFDEVICE_INIT pDeviceInit)
{
    UNREFERENCED_PARAMETER(Driver);

    WDF_PNPPOWER_EVENT_CALLBACKS pnp;
    WDF_PNPPOWER_EVENT_CALLBACKS_INIT(&pnp);
    pnp.EvtDeviceD0Entry = PhantomDeviceD0Entry;
    WdfDeviceInitSetPnpPowerEventCallbacks(pDeviceInit, &pnp);

    IDD_CX_CLIENT_CONFIG iddConfig;
    IDD_CX_CLIENT_CONFIG_INIT(&iddConfig);
    iddConfig.EvtIddCxAdapterInitFinished = PhantomAdapterInitFinished;
    iddConfig.EvtIddCxAdapterCommitModes = PhantomAdapterCommitModes;
    iddConfig.EvtIddCxParseMonitorDescription = PhantomParseMonitorDescription;
    iddConfig.EvtIddCxMonitorGetDefaultDescriptionModes = PhantomMonitorGetDefaultModes;
    iddConfig.EvtIddCxMonitorQueryTargetModes = PhantomMonitorQueryTargetModes;
    iddConfig.EvtIddCxMonitorAssignSwapChain = PhantomMonitorAssignSwapChain;
    iddConfig.EvtIddCxMonitorUnassignSwapChain = PhantomMonitorUnassignSwapChain;

    IddLog("DeviceAdd: IddCxDeviceInitConfig");
    NTSTATUS status = IddCxDeviceInitConfig(pDeviceInit, &iddConfig);
    if (!NT_SUCCESS(status))
    {
        char buf[64]; sprintf_s(buf, "IddCxDeviceInitConfig FAILED: 0x%08X", status); IddLog(buf);
        return status;
    }

    WDF_OBJECT_ATTRIBUTES deviceAttrs;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&deviceAttrs, PhantomDeviceContext);

    WDFDEVICE device = nullptr;
    status = WdfDeviceCreate(&pDeviceInit, &deviceAttrs, &device);
    if (!NT_SUCCESS(status))
    {
        char buf[64]; sprintf_s(buf, "WdfDeviceCreate FAILED: 0x%08X", status); IddLog(buf);
        return status;
    }

    auto* ctx = GetDeviceContext(device);
    ctx->wdfDevice = device;
    g_ctx = ctx;

    status = IddCxDeviceInitialize(device);
    char buf[64]; sprintf_s(buf, "DeviceAdd done: 0x%08X", status); IddLog(buf);
    return status;
}

NTSTATUS PhantomDeviceD0Entry(WDFDEVICE Device, WDF_POWER_DEVICE_STATE PreviousState)
{
    UNREFERENCED_PARAMETER(PreviousState);
    IddLog("D0Entry called");
    auto* ctx = GetDeviceContext(Device);
    ctx->InitAdapter();
    return STATUS_SUCCESS;
}

NTSTATUS PhantomAdapterInitFinished(IDDCX_ADAPTER Adapter, const IDARG_IN_ADAPTER_INIT_FINISHED* pInArgs)
{
    UNREFERENCED_PARAMETER(Adapter);
    char buf[64]; sprintf_s(buf, "AdapterInitFinished: status=0x%08X g_ctx=%p", pInArgs->AdapterInitStatus, g_ctx); IddLog(buf);
    if (NT_SUCCESS(pInArgs->AdapterInitStatus) && g_ctx)
    {
        g_ctx->CreateMonitor();
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
    UNREFERENCED_PARAMETER(pInArgs);
    pOutArgs->MonitorModeBufferOutputCount = 0;
    return STATUS_SUCCESS;
}

NTSTATUS PhantomMonitorGetDefaultModes(IDDCX_MONITOR Monitor,
    const IDARG_IN_GETDEFAULTDESCRIPTIONMODES* pInArgs,
    IDARG_OUT_GETDEFAULTDESCRIPTIONMODES* pOutArgs)
{
    UNREFERENCED_PARAMETER(Monitor);
    {char buf[64]; sprintf_s(buf, "GetDefaultModes: input=%u", pInArgs->DefaultMonitorModeBufferInputCount); IddLog(buf);}

    // Report default 1920x1080 mode.
    if (pInArgs->DefaultMonitorModeBufferInputCount == 0)
    {
        pOutArgs->DefaultMonitorModeBufferOutputCount = 1;
    }
    else
    {
        pInArgs->pDefaultMonitorModes[0] = CreateMonitorMode(DEFAULT_WIDTH, DEFAULT_HEIGHT, DEFAULT_VREFRESH);
        pOutArgs->DefaultMonitorModeBufferOutputCount = 1;
        pOutArgs->PreferredMonitorModeIdx = 0;
    }
    return STATUS_SUCCESS;
}

NTSTATUS PhantomMonitorQueryTargetModes(IDDCX_MONITOR Monitor,
    const IDARG_IN_QUERYTARGETMODES* pInArgs,
    IDARG_OUT_QUERYTARGETMODES* pOutArgs)
{
    UNREFERENCED_PARAMETER(Monitor);
    {char buf[64]; sprintf_s(buf, "QueryTargetModes: input=%u", pInArgs->TargetModeBufferInputCount); IddLog(buf);}

    IDDCX_TARGET_MODE mode = CreateTargetMode(DEFAULT_WIDTH, DEFAULT_HEIGHT, DEFAULT_VREFRESH);

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
    UNREFERENCED_PARAMETER(Monitor);
    UNREFERENCED_PARAMETER(pInArgs);
    return STATUS_SUCCESS;
}

NTSTATUS PhantomMonitorUnassignSwapChain(IDDCX_MONITOR Monitor)
{
    UNREFERENCED_PARAMETER(Monitor);
    return STATUS_SUCCESS;
}
