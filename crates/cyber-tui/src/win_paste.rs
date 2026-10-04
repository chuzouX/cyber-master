//! Windows Terminal Ctrl+V 图片粘贴拦截与捕获模块。
//!
//! 背景：Windows Terminal 会在终端宿主层默认拦截 Ctrl+V 进行文本粘贴。
//! 当剪贴板仅含位图图像（如微信截图、系统截图 Win+Shift+S、浏览器复制图片）时，
//! 由于缺少 CF_UNICODETEXT 文本格式，Windows Terminal 会直接静默丢弃按键，
//! 导致底层的终端应用程序（Pty 子进程）无法收到任何 KeyEvent 或 Paste 事件。
//!
//! 本模块通过前台终端窗口判定与热键上升沿捕获，在 Windows Terminal 下实现无缝 Ctrl+V 图片粘贴。

#[cfg(windows)]
use std::collections::HashSet;
#[cfg(windows)]
use std::sync::LazyLock;
/// 检查当前前台窗口是否属于终端或当前进程。
#[cfg(windows)]
pub fn is_terminal_foreground() -> bool {
    #[link(name = "user32")]
    extern "system" {
        fn GetForegroundWindow() -> usize;
        fn GetWindowThreadProcessId(hWnd: usize, lpdwProcessId: *mut u32) -> u32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetConsoleWindow() -> usize;
        fn GetConsoleProcessList(lpdwProcessList: *mut u32, dwProcessCount: u32) -> u32;
    }

    let fg = unsafe { GetForegroundWindow() };
    if fg == 0 {
        return false;
    }

    let con = unsafe { GetConsoleWindow() };
    if con != 0 && con == fg {
        return true;
    }

    let mut fg_pid = 0u32;
    unsafe { GetWindowThreadProcessId(fg, &mut fg_pid) };
    if fg_pid == 0 {
        return false;
    }

    let ancestors = get_terminal_ancestor_pids();
    if ancestors.contains(&fg_pid) {
        return true;
    }

    let mut pids = [0u32; 64];
    let count = unsafe { GetConsoleProcessList(pids.as_mut_ptr(), 64) };
    (0..count as usize).any(|i| pids[i] == fg_pid)
}

#[cfg(not(windows))]
pub fn is_terminal_foreground() -> bool {
    true
}

/// 缓存终端宿主及祖先进程 ID 集合，避免每帧重复遍历进程快照。
#[cfg(windows)]
static ANCESTORS: LazyLock<HashSet<u32>> = LazyLock::new(|| {
    let mut set = HashSet::new();

    #[repr(C)]
    struct PROCESSENTRY32W {
        dw_size: u32,
        cnt_usage: u32,
        th32_process_id: u32,
        th32_default_heap_id: usize,
        th32_module_id: u32,
        cnt_threads: u32,
        th32_parent_process_id: u32,
        pc_pri_class_base: i32,
        dw_flags: u32,
        sz_exe_file: [u16; 260],
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateToolhelp32Snapshot(dwFlags: u32, th32ProcessID: u32) -> usize;
        fn Process32FirstW(hSnapshot: usize, lppe: *mut PROCESSENTRY32W) -> i32;
        fn Process32NextW(hSnapshot: usize, lppe: *mut PROCESSENTRY32W) -> i32;
        fn CloseHandle(hObject: usize) -> i32;
        fn GetCurrentProcessId() -> u32;
        fn GetConsoleProcessList(lpdwProcessList: *mut u32, dwProcessCount: u32) -> u32;
    }

    let my_pid = unsafe { GetCurrentProcessId() };
    set.insert(my_pid);

    let snapshot = unsafe { CreateToolhelp32Snapshot(0x00000002, 0) };
    if snapshot != 0 && snapshot != usize::MAX {
        let mut parents = std::collections::HashMap::new();
        let mut pe = PROCESSENTRY32W {
            dw_size: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            cnt_usage: 0,
            th32_process_id: 0,
            th32_default_heap_id: 0,
            th32_module_id: 0,
            cnt_threads: 0,
            th32_parent_process_id: 0,
            pc_pri_class_base: 0,
            dw_flags: 0,
            sz_exe_file: [0; 260],
        };

        if unsafe { Process32FirstW(snapshot, &mut pe) } != 0 {
            loop {
                parents.insert(pe.th32_process_id, pe.th32_parent_process_id);
                if unsafe { Process32NextW(snapshot, &mut pe) } == 0 {
                    break;
                }
            }
        }
        unsafe { CloseHandle(snapshot) };

        let mut curr = my_pid;
        let mut visited = HashSet::new();
        while curr != 0 && visited.insert(curr) {
            set.insert(curr);
            curr = parents.get(&curr).copied().unwrap_or(0);
        }

        let mut console_pids = [0u32; 64];
        let count = unsafe { GetConsoleProcessList(console_pids.as_mut_ptr(), 64) };
        for &pid in &console_pids[..count as usize] {
            let mut c_curr = pid;
            let mut c_visited = HashSet::new();
            while c_curr != 0 && c_visited.insert(c_curr) {
                set.insert(c_curr);
                c_curr = parents.get(&c_curr).copied().unwrap_or(0);
            }
        }
    }

    set
});

#[cfg(windows)]
fn get_terminal_ancestor_pids() -> &'static HashSet<u32> {
    &ANCESTORS
}

/// 检查 Ctrl+V 组合键的上升沿（新按下的一瞬间，防长按重复触发）。
#[cfg(windows)]
pub fn check_ctrl_v_rising_edge(was_down: &mut bool) -> bool {
    #[link(name = "user32")]
    extern "system" {
        fn GetAsyncKeyState(vKey: i32) -> i16;
    }

    let ctrl = unsafe { (GetAsyncKeyState(0x11) as u16 & 0x8000) != 0 }; // VK_CONTROL = 0x11
    let v = unsafe { (GetAsyncKeyState(b'V' as i32) as u16 & 0x8000) != 0 };
    let is_down = ctrl && v;
    let rising_edge = is_down && !*was_down;
    *was_down = is_down;

    rising_edge
}

#[cfg(not(windows))]
pub fn check_ctrl_v_rising_edge(_was_down: &mut bool) -> bool {
    false
}
