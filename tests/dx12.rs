mod harness;
mod hook;

use std::thread;
use std::time::Duration;

use harness::dx12::Dx12Harness;
use hook::HookExample;
use hudhook::hooks::dx12::ImguiDx12Hooks;
use hudhook::hooks::{Hooks, ImguiRenderLoop};
use tracing::metadata::LevelFilter;

#[test]
fn test_imgui_dx12() {
    tracing_subscriber::fmt()
        .with_max_level(LevelFilter::TRACE)
        .with_thread_ids(true)
        .with_file(true)
        .with_line_number(true)
        .with_thread_names(true)
        .init();

    let dx12_harness = Dx12Harness::new("DX12 hook example");
    thread::sleep(Duration::from_millis(500));

    unsafe {
        let hooks: Box<dyn Hooks> = { HookExample::new().into_hook::<ImguiDx12Hooks>() };
        hooks.hook();
        hudhook::lifecycle::global_state::set_hooks(hooks);
    }

    thread::sleep(Duration::from_millis(5000));
    drop(dx12_harness);
}
