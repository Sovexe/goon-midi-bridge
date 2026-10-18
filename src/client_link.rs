use std::{
    collections::BTreeSet,
    path::Path,
    time::{Duration, Instant},
};
use windows::{
    core::VARIANT,
    Win32::{
        System::Com::{
            CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
            COINIT_MULTITHREADED,
        },
        UI::Accessibility::*,
    },
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HWND, LPARAM},
    System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    },
    UI::WindowsAndMessaging::*,
};

#[derive(Clone)]
pub struct Client {
    window: usize,
    process: u32,
    pub title: String,
}

fn window_title(window: HWND) -> String {
    let mut buffer = [0u16; 1024];
    let length = unsafe { GetWindowTextW(window, buffer.as_mut_ptr(), buffer.len() as i32) };
    String::from_utf16_lossy(&buffer[..length.max(0) as usize])
}

fn control_text(window: HWND) -> String {
    let mut buffer = vec![0u16; 32768];
    let mut length = 0;
    unsafe {
        SendMessageTimeoutW(
            window,
            WM_GETTEXT,
            buffer.len(),
            buffer.as_mut_ptr() as LPARAM,
            SMTO_ABORTIFHUNG,
            1000,
            &mut length,
        );
    }
    String::from_utf16_lossy(&buffer[..length.min(buffer.len() - 1)])
}

fn is_dreamseeker(process: u32) -> bool {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process);
        if handle.is_null() {
            return false;
        }
        let mut buffer = [0u16; 1024];
        let mut length = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut length);
        CloseHandle(handle);
        if ok == 0 {
            return false;
        }
        let image_path = String::from_utf16_lossy(&buffer[..length as usize]);
        Path::new(&image_path)
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("dreamseeker.exe"))
    }
}

unsafe extern "system" fn collect_client(window: HWND, data: LPARAM) -> i32 {
    if IsWindowVisible(window) == 0 || !GetWindow(window, GW_OWNER).is_null() {
        return 1;
    }
    let mut process = 0;
    GetWindowThreadProcessId(window, &mut process);
    if is_dreamseeker(process) {
        let clients = &mut *(data as *mut Vec<Client>);
        clients.push(Client {
            window: window as usize,
            process,
            title: window_title(window),
        });
    }
    1
}

pub fn clients() -> Vec<Client> {
    let mut result = Vec::new();
    unsafe {
        EnumWindows(Some(collect_client), &mut result as *mut _ as LPARAM);
    }
    result
}

unsafe extern "system" fn collect_input(window: HWND, data: LPARAM) -> i32 {
    let mut class = [0u16; 64];
    let length = GetClassNameW(window, class.as_mut_ptr(), class.len() as i32);
    if IsWindowVisible(window) != 0
        && GetWindowLongW(window, GWL_STYLE) as u32 & WS_DISABLED == 0
        && String::from_utf16_lossy(&class[..length.max(0) as usize]) == "Edit"
    {
        (&mut *(data as *mut Vec<usize>)).push(window as usize);
    }
    1
}

fn output_command(control: &str, script: &str) -> String {
    let encoded: String = script.bytes().map(|byte| format!("%{byte:02X}")).collect();
    format!(".output {control}:eval {encoded}")
}

pub fn command(adapter_url: &str, control: &str) -> String {
    let script = format!(
        r#"(() => {{
            if (!document.querySelector('.instruments__piano')) return;
            const socket = new WebSocket({url});
            socket.onmessage = event => {{
                socket.close();
                (0, eval)(event.data);
            }};
        }})()"#,
        url = serde_json::to_string(adapter_url).unwrap(),
    );
    output_command(control, &script)
}

fn control_from_url(url: &str) -> Option<String> {
    let (port, path) = url.strip_prefix("http://127.0.0.1:")?.split_once('/')?;
    if port.parse::<u16>().ok()? == 0 {
        return None;
    }
    let file = path.split(['?', '#']).next()?.rsplit('/').next()?;
    let id = file.strip_suffix(".html")?;
    let index = id.strip_prefix("tgui-window-")?;
    if index.is_empty()
        || !index.bytes().all(|byte| byte.is_ascii_digit())
        || index.parse::<u32>().ok()? == 0
    {
        return None;
    }
    Some(format!("{id}.browser"))
}

struct WindowSearch {
    process: u32,
    windows: Vec<usize>,
}

unsafe extern "system" fn collect_windows(window: HWND, data: LPARAM) -> i32 {
    let search = &mut *(data as *mut WindowSearch);
    let mut process = 0;
    GetWindowThreadProcessId(window, &mut process);
    if process == search.process && IsWindowVisible(window) != 0 {
        search.windows.push(window as usize);
    }
    1
}

struct ComApartment;
impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

unsafe fn document_url(document: &IUIAutomationElement) -> Option<String> {
    if let Ok(pattern) =
        document.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
    {
        if let Ok(value) = pattern.CurrentValue() {
            return Some(value.to_string());
        }
    }
    document
        .GetCurrentPatternAs::<IUIAutomationLegacyIAccessiblePattern>(
            UIA_LegacyIAccessiblePatternId,
        )
        .ok()?
        .CurrentValue()
        .ok()
        .map(|value| value.to_string())
}

fn instrument_controls(client: &Client) -> Result<Vec<String>, String> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }
        .map_err(|error| format!("Cannot inspect BYOND windows: {error}"))?;
    let _apartment = ComApartment;
    find_instrument_controls(client)
        .map_err(|error| format!("Cannot inspect BYOND instrument pages: {error}"))
}

fn find_instrument_controls(client: &Client) -> windows::core::Result<Vec<String>> {
    unsafe {
        let automation: IUIAutomation =
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
        let documents = automation.CreatePropertyCondition(
            UIA_ControlTypePropertyId,
            &VARIANT::from(UIA_DocumentControlTypeId.0),
        )?;
        let bindings = automation.CreatePropertyCondition(
            UIA_NamePropertyId,
            &VARIANT::from("Key binding order for keyboard input"),
        )?;
        let instructions = automation.CreatePropertyCondition(
            UIA_NamePropertyId,
            &VARIANT::from("Type in the order you wish the keybindings to be placed"),
        )?;
        let mut search = WindowSearch {
            process: client.process,
            windows: Vec::new(),
        };
        EnumWindows(Some(collect_windows), &mut search as *mut _ as LPARAM);
        let mut controls = BTreeSet::new();
        for window in search.windows {
            let root = match automation
                .ElementFromHandle(windows::Win32::Foundation::HWND(window as isize))
            {
                Ok(root) => root,
                Err(_) => continue,
            };
            let pages = match root.FindAll(TreeScope_Descendants, &documents) {
                Ok(pages) => pages,
                Err(_) => continue,
            };
            for index in 0..pages.Length()? {
                let page = pages.GetElement(index)?;
                if page.CurrentIsOffscreen()?.as_bool() {
                    continue;
                }
                let Some(control) = document_url(&page).and_then(|url| control_from_url(&url))
                else {
                    continue;
                };
                if page.FindFirst(TreeScope_Descendants, &bindings).is_ok()
                    && page.FindFirst(TreeScope_Descendants, &instructions).is_ok()
                {
                    controls.insert(control);
                }
            }
        }
        Ok(controls.into_iter().collect())
    }
}

unsafe fn command_menu(menu: HMENU) -> Option<u32> {
    for index in 0..GetMenuItemCount(menu) {
        let mut label = [0u16; 128];
        let length = GetMenuStringW(
            menu,
            index as u32,
            label.as_mut_ptr(),
            label.len() as i32,
            MF_BYPOSITION,
        );
        if String::from_utf16_lossy(&label[..length.max(0) as usize]).replace('&', "")
            == "Run Command"
        {
            return Some(GetMenuItemID(menu, index));
        }
        let submenu = GetSubMenu(menu, index);
        if !submenu.is_null() {
            if let Some(command) = command_menu(submenu) {
                return Some(command);
            }
        }
    }
    None
}

struct DialogSearch {
    process: u32,
    dialog: usize,
}

unsafe extern "system" fn collect_dialog(window: HWND, data: LPARAM) -> i32 {
    let search = &mut *(data as *mut DialogSearch);
    let mut process = 0;
    GetWindowThreadProcessId(window, &mut process);
    if process == search.process
        && IsWindowVisible(window) != 0
        && window_title(window) == "Enter Command"
    {
        search.dialog = window as usize;
    }
    1
}

fn command_dialog(process: u32) -> HWND {
    let mut search = DialogSearch { process, dialog: 0 };
    unsafe {
        EnumWindows(Some(collect_dialog), &mut search as *mut _ as LPARAM);
    }
    search.dialog as HWND
}

struct CommandDialog {
    window: HWND,
    process: u32,
}

impl Drop for CommandDialog {
    fn drop(&mut self) {
        unsafe {
            let mut process = 0;
            GetWindowThreadProcessId(self.window, &mut process);
            if process == self.process && window_title(self.window) == "Enter Command" {
                PostMessageW(self.window, WM_CLOSE, 0, 0);
            }
        }
    }
}

unsafe extern "system" fn collect_ok(window: HWND, data: LPARAM) -> i32 {
    let mut class = [0u16; 64];
    let length = GetClassNameW(window, class.as_mut_ptr(), class.len() as i32);
    if String::from_utf16_lossy(&class[..length.max(0) as usize]) == "Button"
        && control_text(window).replace('&', "") == "OK"
    {
        *(data as *mut usize) = window as usize;
    }
    1
}

pub fn link(client: &Client, adapter_url: &str) -> Result<usize, String> {
    let window = client.window as HWND;
    let mut process = 0;
    unsafe {
        GetWindowThreadProcessId(window, &mut process);
    }
    if process != client.process || !is_dreamseeker(process) {
        return Err("The selected BYOND client closed. Refresh the list.".into());
    }
    let controls = instrument_controls(client)?;
    if controls.is_empty() {
        return Err(
            "No open instrument windows found. Restore an instrument window and try again.".into(),
        );
    }
    if !command_dialog(process).is_null() {
        return Err("Close BYOND's open Run Command dialog before linking.".into());
    }
    let command_id = unsafe { command_menu(GetMenu(window)) }
        .ok_or("This BYOND window has no File / Run Command menu.")?;
    for control in &controls {
        let command = command(adapter_url, control);
        let dialog = open_command_dialog(client, command_id)?;
        submit_command(&dialog, &command)?;
    }
    Ok(controls.len())
}

fn open_command_dialog(client: &Client, command_id: u32) -> Result<CommandDialog, String> {
    let window = client.window as HWND;
    let mut process = 0;
    unsafe {
        GetWindowThreadProcessId(window, &mut process);
        if process != client.process {
            return Err("The selected BYOND client closed.".into());
        }
        if GetWindowLongW(window, GWL_STYLE) as u32 & WS_DISABLED != 0 {
            return Err("Close the open BYOND dialog before linking.".into());
        }
        if PostMessageW(window, WM_COMMAND, command_id as usize, 0) == 0 {
            return Err("Could not open BYOND Run Command.".into());
        }
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    let dialog = loop {
        let dialog = command_dialog(process);
        if !dialog.is_null() {
            break CommandDialog {
                window: dialog,
                process,
            };
        }
        if Instant::now() >= deadline {
            return Err("BYOND Run Command dialog was not found.".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Ok(dialog)
}

fn submit_command(dialog: &CommandDialog, command: &str) -> Result<(), String> {
    let mut inputs = Vec::<usize>::new();
    let mut ok_button = 0usize;
    unsafe {
        EnumChildWindows(
            dialog.window,
            Some(collect_input),
            &mut inputs as *mut _ as LPARAM,
        );
        EnumChildWindows(
            dialog.window,
            Some(collect_ok),
            &mut ok_button as *mut _ as LPARAM,
        );
    }
    let [input] = inputs.as_slice() else {
        return Err("Unrecognized BYOND command dialog.".into());
    };
    if ok_button == 0 {
        return Err("BYOND command dialog has no OK button.".into());
    }
    let input = *input as HWND;
    if !control_text(input).is_empty() {
        return Err("The command dialog changed during linking.".into());
    }
    let wide: Vec<u16> = command.encode_utf16().chain(Some(0)).collect();
    let mut result = 0;
    unsafe {
        if SendMessageTimeoutW(
            input,
            WM_SETTEXT,
            0,
            wide.as_ptr() as LPARAM,
            SMTO_ABORTIFHUNG,
            1000,
            &mut result,
        ) == 0
            || result == 0
        {
            return Err("BYOND did not accept the link command.".into());
        }
        if control_text(input) != command {
            return Err("BYOND truncated the link command.".into());
        }
        if PostMessageW(
            dialog.window,
            WM_COMMAND,
            GetDlgCtrlID(ok_button as HWND) as usize,
            ok_button as LPARAM,
        ) == 0
        {
            return Err("Could not submit the BYOND command.".into());
        }
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while unsafe { IsWindow(dialog.window) != 0 && IsWindowVisible(dialog.window) != 0 } {
        if control_text(input).is_empty() {
            unsafe {
                PostMessageW(dialog.window, WM_CLOSE, 0, 0);
            }
        }
        if Instant::now() >= deadline {
            return Err("BYOND did not finish the link command.".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}
