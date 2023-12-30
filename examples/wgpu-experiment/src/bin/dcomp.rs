use std::fs::File;
use std::mem::{ManuallyDrop, MaybeUninit};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{bail, Context as AnyhowCtx, Result};
use imgui::Context;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::prelude::*;
use wgpu_experiment::imgui_dx12::RenderEngine;
use wgpu_experiment::try_out_param;
use windows::core::{w, ComInterface, PCWSTR};
use windows::Win32::Foundation::{
    BOOL, COLORREF, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Direct3D::{D3D_FEATURE_LEVEL_12_0, D3D_FEATURE_LEVEL_12_2};
use windows::Win32::Graphics::Direct3D12::{
    D3D12CreateDevice, ID3D12CommandAllocator, ID3D12CommandQueue, ID3D12DescriptorHeap,
    ID3D12Device, ID3D12Fence, ID3D12GraphicsCommandList, ID3D12Resource,
    D3D12_COMMAND_LIST_TYPE_DIRECT, D3D12_COMMAND_QUEUE_DESC, D3D12_COMMAND_QUEUE_FLAG_NONE,
    D3D12_CPU_DESCRIPTOR_HANDLE, D3D12_DESCRIPTOR_HEAP_DESC, D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
    D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
    D3D12_DESCRIPTOR_HEAP_TYPE_RTV, D3D12_FENCE_FLAG_NONE, D3D12_RESOURCE_BARRIER,
    D3D12_RESOURCE_BARRIER_0, D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
    D3D12_RESOURCE_BARRIER_FLAG_NONE, D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
    D3D12_RESOURCE_STATE_PRESENT, D3D12_RESOURCE_STATE_RENDER_TARGET,
    D3D12_RESOURCE_TRANSITION_BARRIER,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_MODE_DESC, DXGI_MODE_SCALING_UNSPECIFIED,
    DXGI_MODE_SCANLINE_ORDER_UNSPECIFIED, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory, CreateDXGIFactory2, DXGIGetDebugInterface1, IDXGIAdapter, IDXGIFactory,
    IDXGIFactory2, IDXGIInfoQueue, IDXGISwapChain, IDXGISwapChain3, DXGI_ADAPTER_DESC,
    DXGI_CREATE_FACTORY_DEBUG, DXGI_DEBUG_ALL, DXGI_INFO_QUEUE_MESSAGE, DXGI_SWAP_CHAIN_DESC,
    DXGI_SWAP_CHAIN_FLAG_ALLOW_MODE_SWITCH, DXGI_SWAP_EFFECT_FLIP_DISCARD,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::Graphics::Gdi::{ScreenToClient, UpdateWindow};
use windows::Win32::System::Threading::{
    CreateEventExW, WaitForSingleObjectEx, CREATE_EVENT, INFINITE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageA, GetClientRect, GetCursorPos,
    GetForegroundWindow, GetMessageA, GetWindowRect, IsChild, RegisterClassExW,
    SetLayeredWindowAttributes, TranslateMessage, CS_HREDRAW, CS_VREDRAW, LWA_COLORKEY, WM_CLOSE,
    WM_QUIT, WNDCLASSEXW, WS_CAPTION, WS_EX_APPWINDOW, WS_EX_LAYERED, WS_EX_TRANSPARENT, WS_POPUP,
    WS_VISIBLE,
};

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;

type WndProcType =
    unsafe extern "system" fn(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT;

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

#[derive(Debug)]
struct FrameContext {
    back_buffer: ID3D12Resource,
    desc_handle: D3D12_CPU_DESCRIPTOR_HANDLE,
    command_allocator: ID3D12CommandAllocator,
    fence: ID3D12Fence,
    fence_val: u64,
    fence_event: HANDLE,
}

impl FrameContext {
    fn incr(&mut self) {
        static FENCE_MAX: AtomicU64 = AtomicU64::new(0);
        self.fence_val = FENCE_MAX.fetch_add(1, Ordering::SeqCst);
    }

    fn wait_fence(&mut self) {
        unsafe {
            if self.fence.GetCompletedValue() < self.fence_val {
                self.fence.SetEventOnCompletion(self.fence_val, self.fence_event).unwrap();
                WaitForSingleObjectEx(self.fence_event, INFINITE, false);
            }
        }
    }
}

struct Dcomp {
    target_hwnd: HWND,
    dxgi_factory: IDXGIFactory2,
    dxgi_adapter: IDXGIAdapter,
    d3d12_dev: ID3D12Device,
    swap_chain: IDXGISwapChain3,

    command_queue: ID3D12CommandQueue,
    command_list: ID3D12GraphicsCommandList,
    renderer_heap: ID3D12DescriptorHeap,
    rtv_heap: ID3D12DescriptorHeap,

    // dcomp_dev: IDCompositionDevice,
    // dcomp_target: IDCompositionTarget,
    // root_visual: IDCompositionVisual,
    engine: RenderEngine,
    ctx: Context,
    frame_contexts: Vec<FrameContext>,
}

impl Dcomp {
    unsafe fn new(target_hwnd: HWND) -> Result<Self> {
        let dxgi_factory: IDXGIFactory2 =
            CreateDXGIFactory2(DXGI_CREATE_FACTORY_DEBUG).context("dxgi factory")?;

        let dxgi_adapter = dxgi_factory.EnumAdapters(0).context("enum adapters")?;

        let mut d3d12_dev: Option<ID3D12Device> = None;
        D3D12CreateDevice(&dxgi_adapter, D3D_FEATURE_LEVEL_12_2, &mut d3d12_dev)
            .context("create device")?;
        let d3d12_dev = d3d12_dev.unwrap();

        let queue_desc = D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
            Priority: 0,
            Flags: D3D12_COMMAND_QUEUE_FLAG_NONE,
            NodeMask: 0,
        };

        let command_queue: ID3D12CommandQueue =
            unsafe { d3d12_dev.CreateCommandQueue(&queue_desc as *const _) }.unwrap();

        let (width, height) = win_size(target_hwnd);

        let sd = DXGI_SWAP_CHAIN_DESC {
            BufferDesc: DXGI_MODE_DESC {
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                ScanlineOrdering: DXGI_MODE_SCANLINE_ORDER_UNSPECIFIED,
                Scaling: DXGI_MODE_SCALING_UNSPECIFIED,
                Width: width as _,
                Height: height as _,
                RefreshRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            OutputWindow: target_hwnd,
            Windowed: BOOL(1),
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Flags: Default::default(), // DXGI_SWAP_CHAIN_FLAG_ALLOW_MODE_SWITCH.0 as _,
        };

        let mut swap_chain = None;
        dxgi_factory
            .CreateSwapChain(&command_queue, &sd, &mut swap_chain)
            .ok()
            .context("create swap chain")?;
        let swap_chain =
            swap_chain.unwrap().cast::<IDXGISwapChain3>().ok().context("query interface")?;

        let renderer_heap: ID3D12DescriptorHeap = unsafe {
            d3d12_dev
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: sd.BufferCount,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
                .context("create descriptor heap")?
        };

        let command_allocator: ID3D12CommandAllocator = d3d12_dev
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .context("create command allocator")?;

        let command_list: ID3D12GraphicsCommandList = d3d12_dev
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &command_allocator, None)
            .unwrap();
        command_list.Close().unwrap();

        command_list.SetName(w!("hudhook Command List")).expect("Couldn't set command list name");

        let rtv_heap: ID3D12DescriptorHeap = d3d12_dev
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                NumDescriptors: sd.BufferCount,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
                NodeMask: 1,
            })
            .unwrap();

        let rtv_heap_inc_size =
            d3d12_dev.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV);

        let rtv_handle_start = rtv_heap.GetCPUDescriptorHandleForHeapStart();

        let frame_contexts: Vec<FrameContext> = (0..sd.BufferCount)
            .map(|i| {
                const COMMAND_ALLOCATOR_NAMES: [PCWSTR; 8] = [
                    w!("hudhook Command allocator #0"),
                    w!("hudhook Command allocator #1"),
                    w!("hudhook Command allocator #2"),
                    w!("hudhook Command allocator #3"),
                    w!("hudhook Command allocator #4"),
                    w!("hudhook Command allocator #5"),
                    w!("hudhook Command allocator #6"),
                    w!("hudhook Command allocator #7"),
                ];

                let desc_handle = D3D12_CPU_DESCRIPTOR_HANDLE {
                    ptr: rtv_handle_start.ptr + (i * rtv_heap_inc_size) as usize,
                };

                let back_buffer: ID3D12Resource = swap_chain.GetBuffer(i).context("get buffer")?;
                d3d12_dev.CreateRenderTargetView(&back_buffer, None, desc_handle);

                let command_allocator: ID3D12CommandAllocator =
                    d3d12_dev.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
                let command_allocator_name = COMMAND_ALLOCATOR_NAMES
                    [usize::min(COMMAND_ALLOCATOR_NAMES.len() - 1, i as usize)];

                command_allocator
                    .SetName(command_allocator_name)
                    .context("Couldn't set command allocator name")?;

                Ok(FrameContext {
                    desc_handle,
                    back_buffer,
                    command_allocator,
                    fence: d3d12_dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap(),
                    fence_val: 0,
                    fence_event: CreateEventExW(None, None, CREATE_EVENT(0), 0x1F0003).unwrap(),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        println!("{frame_contexts:?}");

        let mut ctx = Context::create();
        let cpu_desc = renderer_heap.GetCPUDescriptorHandleForHeapStart();
        let gpu_desc = renderer_heap.GetGPUDescriptorHandleForHeapStart();
        let engine = RenderEngine::new(
            &mut ctx,
            d3d12_dev.clone(),
            sd.BufferCount,
            DXGI_FORMAT_R8G8B8A8_UNORM,
            renderer_heap.clone(),
            cpu_desc,
            gpu_desc,
        );

        // let dcomp_dev: IDCompositionDevice =
        //     DCompositionCreateDevice(None).context("create dcomp device")?;
        // let dcomp_target = dcomp_dev
        //     .CreateTargetForHwnd(target_hwnd, BOOL::from(true))
        //     .context("create target for hwnd")?;
        //
        // let root_visual = dcomp_dev.CreateVisual().context("create visual")?;
        // dcomp_target.SetRoot(&root_visual)?;

        Ok(Self {
            target_hwnd,
            dxgi_factory,
            dxgi_adapter,
            d3d12_dev,
            swap_chain,
            command_queue,
            command_list,
            renderer_heap,
            rtv_heap,
            // dcomp_dev,
            // dcomp_target,
            // root_visual,
            engine,
            ctx,
            frame_contexts,
        })
    }

    unsafe fn render(&mut self) -> Result<()> {
        let render_start = Instant::now();

        let frame_contexts_idx = unsafe { self.swap_chain.GetCurrentBackBufferIndex() } as usize;
        let frame_context = &mut self.frame_contexts[frame_contexts_idx];

        let sd = try_out_param(|sd| unsafe { self.swap_chain.GetDesc(sd) }).context("GetDesc")?;
        let rect: Result<RECT, _> =
            try_out_param(|rect| unsafe { GetClientRect(sd.OutputWindow, rect) });

        match rect {
            Ok(rect) => {
                let io = self.ctx.io_mut();

                io.display_size =
                    [(rect.right - rect.left) as f32, (rect.bottom - rect.top) as f32];

                let mut pos = POINT { x: 0, y: 0 };

                let active_window = unsafe { GetForegroundWindow() };
                if !HANDLE(active_window.0).is_invalid()
                    && (active_window == sd.OutputWindow
                        || unsafe { IsChild(active_window, sd.OutputWindow) }.as_bool())
                {
                    let gcp = unsafe { GetCursorPos(&mut pos as *mut _) };
                    if gcp.is_ok()
                        && unsafe { ScreenToClient(sd.OutputWindow, &mut pos as *mut _) }.as_bool()
                    {
                        io.mouse_pos[0] = pos.x as _;
                        io.mouse_pos[1] = pos.y as _;
                    }
                }
            },
            Err(e) => {
                eprintln!("GetClientRect error: {e:?}");
            },
        }

        self.engine.new_frame(&mut self.ctx);
        let ctx = &mut self.ctx;
        let ui = ctx.frame();
        ui.show_demo_window(&mut true);
        // unsafe { IMGUI_RENDER_LOOP.get_mut() }.unwrap().render(ui);
        let draw_data = ctx.render();

        let back_buffer = ManuallyDrop::new(Some(frame_context.back_buffer.clone()));
        let transition_barrier = ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
            pResource: back_buffer,
            Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
            StateBefore: D3D12_RESOURCE_STATE_PRESENT,
            StateAfter: D3D12_RESOURCE_STATE_RENDER_TARGET,
        });

        let mut barrier = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 { Transition: transition_barrier },
        };

        frame_context.wait_fence();
        frame_context.incr();
        let command_allocator = &frame_context.command_allocator;

        unsafe {
            command_allocator.Reset().unwrap();
            self.command_list.Reset(command_allocator, None).unwrap();
            self.command_list.ResourceBarrier(&[barrier.clone()]);
            self.command_list.OMSetRenderTargets(
                1,
                Some(&frame_context.desc_handle),
                BOOL::from(false),
                None,
            );
            self.command_list.SetDescriptorHeaps(&[Some(self.renderer_heap.clone())]);
        };

        if let Err(e) =
            self.engine.render_draw_data(draw_data, &self.command_list, frame_contexts_idx)
        {
            eprintln!("{}", e);
        };

        // Explicit auto deref necessary because this is ManuallyDrop.
        #[allow(clippy::explicit_auto_deref)]
        unsafe {
            (*barrier.Anonymous.Transition).StateBefore = D3D12_RESOURCE_STATE_RENDER_TARGET;
            (*barrier.Anonymous.Transition).StateAfter = D3D12_RESOURCE_STATE_PRESENT;
        }

        let barriers = vec![barrier];

        unsafe {
            self.command_list.ResourceBarrier(&barriers);
            self.command_list.Close().unwrap();
            self.command_queue.ExecuteCommandLists(&[Some(self.command_list.cast().unwrap())]);
            self.command_queue.Signal(&frame_context.fence, frame_context.fence_val).unwrap();
        }

        let barrier = barriers.into_iter().next().unwrap();

        let transition = ManuallyDrop::into_inner(unsafe { barrier.Anonymous.Transition });
        let _ = ManuallyDrop::into_inner(transition.pResource);

        self.swap_chain.Present(1, 0).ok()?;

        Ok(())
    }
}

unsafe fn create_window() -> HWND {
    let wndclassex = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(window_proc),
        lpszMenuName: w!("OverlayClass"),
        lpszClassName: w!("OverlayClass"),

        ..Default::default()
    };

    RegisterClassExW(&wndclassex);

    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TRANSPARENT,
        w!("OverlayClass"),
        w!("OverlayClass"),
        WS_VISIBLE | WS_POPUP,
        100,
        100,
        WIDTH as i32,
        HEIGHT as i32,
        None,
        None,
        None,
        None,
    );

    SetLayeredWindowAttributes(hwnd, COLORREF(0), 0, LWA_COLORKEY).unwrap();

    hwnd
}

unsafe fn print_dxgi_debug_messages() {
    let diq: IDXGIInfoQueue = DXGIGetDebugInterface1(0).unwrap();

    for i in 0..diq.GetNumStoredMessages(DXGI_DEBUG_ALL) {
        let mut msg_len: usize = 0;
        diq.GetMessage(DXGI_DEBUG_ALL, i, None, &mut msg_len as _).unwrap();
        let diqm = vec![0u8; msg_len];
        let pdiqm = diqm.as_ptr() as *mut DXGI_INFO_QUEUE_MESSAGE;
        diq.GetMessage(DXGI_DEBUG_ALL, i, Some(pdiqm), &mut msg_len as _).unwrap();
        let diqm = pdiqm.as_ref().unwrap();
        eprintln!(
            "[DIQ] {}",
            String::from_utf8_lossy(std::slice::from_raw_parts(
                diqm.pDescription,
                diqm.DescriptionByteLength - 1
            ))
        );
    }
    diq.ClearStoredMessages(DXGI_DEBUG_ALL);
}

fn win_size(hwnd: HWND) -> (i32, i32) {
    let mut rect = RECT::default();
    unsafe { GetClientRect(hwnd, &mut rect).unwrap() };
    (rect.right - rect.left, rect.bottom - rect.top)
}

fn handle_message(window: HWND) -> bool {
    unsafe {
        let mut msg = MaybeUninit::uninit();
        if GetMessageA(msg.as_mut_ptr(), window, 0, 0).0 > 0 {
            TranslateMessage(msg.as_ptr());
            DispatchMessageA(msg.as_ptr());
            msg.as_ptr()
                .as_ref()
                .map(|m| m.message != WM_QUIT && m.message != WM_CLOSE)
                .unwrap_or(true)
        } else {
            false
        }
    }
}

fn run() -> Result<()> {
    let hwnd = unsafe { create_window() };
    let mut dcomp = unsafe { Dcomp::new(hwnd)? };

    loop {
        unsafe { dcomp.render()? };
        if !handle_message(hwnd) {
            break;
        }
    }

    Ok(())
}

fn main() {
    let log_file = File::create("foo.log").unwrap();

    let file_layer = tracing_subscriber::fmt::layer()
        .with_thread_ids(true)
        .with_file(true)
        .with_line_number(true)
        .with_thread_names(true)
        .with_writer(Mutex::new(log_file))
        .with_ansi(false)
        .boxed();

    tracing_subscriber::registry().with(LevelFilter::TRACE).with(file_layer).init();
    if let Err(e) = run() {
        eprintln!("{e:?}");
        unsafe { print_dxgi_debug_messages() };
    }
}
