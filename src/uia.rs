//! Windows UI Automation: the tree / find / action surface behind
//! /a11y/{tree,find,action}.
//!
//! The daemon already had `apps` (a pid scan); this adds the rest, producing
//! the SAME text formats the Linux (AT-SPI) backend prints — tab-separated
//! `apps`, box-drawing `tree`, and `w,h` / `x,y` / `w,h,x,y` for find — so the
//! worker can treat every OS identically.

use windows::core::Interface;
use windows::Win32::UI::Accessibility::{
    IUIAutomationElement, IUIAutomationInvokePattern, IUIAutomationTogglePattern,
    IUIAutomationTreeWalker, IUIAutomationValuePattern, TreeScope_Children, UIA_AppBarControlTypeId,
    UIA_ButtonControlTypeId, UIA_CalendarControlTypeId, UIA_CheckBoxControlTypeId,
    UIA_ComboBoxControlTypeId, UIA_DataItemControlTypeId,
    UIA_DocumentControlTypeId, UIA_EditControlTypeId, UIA_GroupControlTypeId,
    UIA_HeaderControlTypeId, UIA_HeaderItemControlTypeId, UIA_HyperlinkControlTypeId,
    UIA_ImageControlTypeId, UIA_InvokePatternId, UIA_ListItemControlTypeId,
    UIA_MenuBarControlTypeId, UIA_MenuControlTypeId, UIA_MenuItemControlTypeId,
    UIA_PaneControlTypeId, UIA_ProgressBarControlTypeId, UIA_RadioButtonControlTypeId,
    UIA_ScrollBarControlTypeId, UIA_SeparatorControlTypeId, UIA_SliderControlTypeId,
    UIA_SpinnerControlTypeId, UIA_SplitButtonControlTypeId, UIA_StatusBarControlTypeId,
    UIA_TabControlTypeId, UIA_TabItemControlTypeId, UIA_TableControlTypeId,
    UIA_TextControlTypeId, UIA_ThumbControlTypeId, UIA_TitleBarControlTypeId,
    UIA_TogglePatternId, UIA_ToolBarControlTypeId, UIA_TreeItemControlTypeId,
    UIA_ValuePatternId, UIA_WindowControlTypeId,
};

/// control_role maps a UIA control type id to the lower_snake_case role name
/// the other backends use.
fn control_role(ct: i32) -> &'static str {
    match ct {
        x if x == UIA_ButtonControlTypeId.0 => "button",
        x if x == UIA_EditControlTypeId.0 => "text_field",
        x if x == UIA_TextControlTypeId.0 => "label",
        x if x == UIA_WindowControlTypeId.0 => "window",
        x if x == UIA_PaneControlTypeId.0 => "panel",
        x if x == UIA_DocumentControlTypeId.0 => "document",
        x if x == UIA_ListItemControlTypeId.0 => "list_item",
        x if x == UIA_MenuItemControlTypeId.0 => "menu_item",
        x if x == UIA_MenuControlTypeId.0 => "menu",
        x if x == UIA_MenuBarControlTypeId.0 => "menu_bar",
        x if x == UIA_CheckBoxControlTypeId.0 => "check_box",
        x if x == UIA_RadioButtonControlTypeId.0 => "radio_button",
        x if x == UIA_ComboBoxControlTypeId.0 => "combo_box",
        x if x == UIA_SliderControlTypeId.0 => "slider",
        x if x == UIA_HyperlinkControlTypeId.0 => "link",
        x if x == UIA_TabControlTypeId.0 => "tab_list",
        x if x == UIA_TabItemControlTypeId.0 => "tab",
        x if x == UIA_TreeItemControlTypeId.0 => "tree_item",
        x if x == UIA_ScrollBarControlTypeId.0 => "scroll_bar",
        x if x == UIA_ToolBarControlTypeId.0 => "tool_bar",
        x if x == UIA_StatusBarControlTypeId.0 => "status_bar",
        x if x == UIA_GroupControlTypeId.0 => "group",
        x if x == UIA_ImageControlTypeId.0 => "image",
        x if x == UIA_TitleBarControlTypeId.0 => "title_bar",
        x if x == UIA_ProgressBarControlTypeId.0 => "progress_bar",
        x if x == UIA_SpinnerControlTypeId.0 => "spinner",
        x if x == UIA_SeparatorControlTypeId.0 => "separator",
        x if x == UIA_HeaderControlTypeId.0 => "header",
        x if x == UIA_HeaderItemControlTypeId.0 => "header_item",
        x if x == UIA_TableControlTypeId.0 => "table",
        x if x == UIA_DataItemControlTypeId.0 => "data_item",
        x if x == UIA_CalendarControlTypeId.0 => "calendar",
        x if x == UIA_ThumbControlTypeId.0 => "thumb",
        x if x == UIA_SplitButtonControlTypeId.0 => "split_button",
        x if x == UIA_AppBarControlTypeId.0 => "app_bar",
        _ => "custom",
    }
}

/// A flattened UIA node: everything the text formats need.
struct Node {
    role: String,
    name: String,
    value: String,
    rect: (i32, i32, i32, i32), // x,y,w,h
    el: IUIAutomationElement,
    kids: Vec<usize>,
}

/// ElementInfo is the snapshot of one UIA element the text formats use.
fn info(el: &IUIAutomationElement) -> (String, String, String, (i32, i32, i32, i32)) {
    unsafe {
        let ct = el.CurrentControlType().map(|c| c.0).unwrap_or(0);
        let role = control_role(ct).to_string();
        let name = el.CurrentName().map(|s| s.to_string()).unwrap_or_default();
        // A value lives behind the ValuePattern (edit/text controls).
        let mut value = String::new();
        if let Ok(p) = el.GetCurrentPattern(UIA_ValuePatternId) {
            if let Ok(vp) = p.cast::<IUIAutomationValuePattern>() {
                value = vp.CurrentValue().map(|s| s.to_string()).unwrap_or_default();
            }
        }
        let r = el.CurrentBoundingRectangle().unwrap_or_default();
        let rect = (r.left, r.top, r.right - r.left, r.bottom - r.top);
        (role, name, value, rect)
    }
}

/// walk flattens the subtree under `root` (RawViewWalker, bounded by `depth`)
/// into `nodes`, returning the index of the root.
fn walk(
    walker: &IUIAutomationTreeWalker,
    el: &IUIAutomationElement,
    depth: u32,
    nodes: &mut Vec<Node>,
) -> usize {
    let (role, name, value, rect) = info(el);
    let idx = nodes.len();
    nodes.push(Node { role, name, value, rect, el: el.clone(), kids: Vec::new() });
    if depth == 0 {
        return idx;
    }
    unsafe {
        let mut child = match walker.GetFirstChildElement(el) {
            Ok(c) => c,
            Err(_) => return idx,
        };
        loop {
            let ci = walk(walker, &child, depth - 1, nodes);
            nodes[idx].kids.push(ci);
            child = match walker.GetNextSiblingElement(&child) {
                Ok(c) => c,
                Err(_) => break,
            };
        }
    }
    idx
}

/// render_tree prints the box-drawing tree the other backends emit.
fn render_tree(nodes: &[Node], root: usize, lines: &mut Vec<String>, prefix: &str) {
    let n = &nodes[root];
    let mut label = n.role.clone();
    if !n.name.is_empty() {
        label.push_str(&format!(" \"{}\"", n.name));
    }
    if !n.value.is_empty() && n.value != n.name {
        label.push_str(&format!(" = \"{}\"", n.value));
    }
    lines.push(format!("{prefix}{label}"));
    // Mirror the macOS/Linux backends exactly: a child's prefix is the
    // parent's connector column replaced by its continuation (`│   ` for a
    // non-last child, spaces for the last) plus the child's own connector.
    let child_base = if let Some(b) = prefix.strip_suffix("├── ") {
        format!("{b}│   ")
    } else if let Some(b) = prefix.strip_suffix("└── ") {
        format!("{b}    ")
    } else {
        prefix.to_string()
    };
    for (i, &k) in n.kids.iter().enumerate() {
        let last = i + 1 == n.kids.len();
        let branch = if last { "└── " } else { "├── " };
        render_tree(nodes, k, lines, &format!("{child_base}{branch}"));
    }
}

impl crate::win::Uia {
    /// element_for_pid returns the top-level window of `pid` (or the focused
    /// one when pid is 0).
    fn element_for_pid(&self, pid: u32) -> Result<IUIAutomationElement, String> {
        unsafe {
            if pid != 0 {
                let cond = self
                    .auto
                    .CreatePropertyCondition(
                        windows::Win32::UI::Accessibility::UIA_ProcessIdPropertyId,
                        &windows::core::VARIANT::from(pid),
                    )
                    .map_err(|e| e.to_string())?;
                let root = self.auto.GetRootElement().map_err(|e| e.to_string())?;
                if let Ok(el) = root.FindFirst(TreeScope_Children, &cond) {
                    return Ok(el);
                }
                // Fall back to the desktop root (a specific pid can be absent
                // from the top-level children when its window is a popup).
                return self.auto.GetRootElement().map_err(|e| e.to_string());
            }
            // pid=0: the DESKTOP ROOT. `ElementFromHandle(GetForegroundWindow())`
            // returns UIA_E_ELEMENTNOTAVAILABLE (0x80040201) in this headless
            // guest — the foreground HWND has no UIA element.
            self.auto.GetRootElement().map_err(|e| e.to_string())
        }
    }

    pub fn tree(&self, pid: u32, depth: u32) -> Result<String, String> {
        let root = self.element_for_pid(pid)?;
        let walker: IUIAutomationTreeWalker =
            unsafe { self.auto.RawViewWalker().map_err(|e| e.to_string())? };
        let mut nodes = Vec::new();
        let r = walk(&walker, &root, if depth == 0 { 8 } else { depth }, &mut nodes);
        let mut lines = Vec::new();
        render_tree(&nodes, r, &mut lines, "");
        Ok(lines.join("\n"))
    }

    pub fn find(&self, pid: u32, sel: &str, out: u8) -> Result<String, String> {
        let root = self.element_for_pid(pid)?;
        let walker: IUIAutomationTreeWalker =
            unsafe { self.auto.RawViewWalker().map_err(|e| e.to_string())? };
        let mut nodes = Vec::new();
        let r = walk(&walker, &root, 12, &mut nodes);
        let mut hits = Vec::new();
        collect_matches(&nodes, r, sel, &mut hits);
        let mut lines = Vec::new();
        for &i in &hits {
            let n = &nodes[i];
            match out {
                1 => lines.push(format!("{},{},{},{}", n.rect.0, n.rect.1, n.rect.2, n.rect.3)),
                2 => lines.push(format!("{},{}", n.rect.0 + n.rect.2 / 2, n.rect.1 + n.rect.3 / 2)),
                3 => lines.push(n.value.clone()),
                _ => lines.push(format!("{} \"{}\"", n.role, n.name)),
            }
        }
        Ok(lines.join("\n"))
    }

    pub fn action(&self, pid: u32, sel: &str, action: &str, value: Option<&str>) -> Result<String, String> {
        let root = self.element_for_pid(pid)?;
        let walker: IUIAutomationTreeWalker =
            unsafe { self.auto.RawViewWalker().map_err(|e| e.to_string())? };
        let mut nodes = Vec::new();
        let r = walk(&walker, &root, 12, &mut nodes);
        let mut hits = Vec::new();
        collect_matches(&nodes, r, sel, &mut hits);
        let i = *hits.first().ok_or("no element matched")?;
        let el = &nodes[i].el;
        unsafe {
            match action {
                "focus" => el.SetFocus().map_err(|e| e.to_string())?,
                "set_value" | "type_text" => {
                    let p = el.GetCurrentPattern(UIA_ValuePatternId).map_err(|e| e.to_string())?;
                    let vp: IUIAutomationValuePattern = p.cast().map_err(|e| e.to_string())?;
                    vp.SetValue(&windows::core::BSTR::from(value.unwrap_or("")))
                        .map_err(|e| e.to_string())?;
                }
                "toggle" => {
                    let p = el.GetCurrentPattern(UIA_TogglePatternId).map_err(|e| e.to_string())?;
                    let tp: IUIAutomationTogglePattern = p.cast().map_err(|e| e.to_string())?;
                    tp.Toggle().map_err(|e| e.to_string())?;
                }
                _ => {
                    let p = el.GetCurrentPattern(UIA_InvokePatternId).map_err(|e| e.to_string())?;
                    let ip: IUIAutomationInvokePattern = p.cast().map_err(|e| e.to_string())?;
                    ip.Invoke().map_err(|e| e.to_string())?;
                }
            }
        }
        Ok("ok".into())
    }
}

/// collect_matches walks the flattened tree looking for `sel`. The selector
/// grammar matches the other backends: `*` (any), a role name, `role[name]`,
/// or `name=NAME` / a bare quoted name.
fn collect_matches(nodes: &[Node], root: usize, sel: &str, out: &mut Vec<usize>) {
    let n = &nodes[root];
    if matches(n, sel) {
        out.push(root);
    }
    for &k in &n.kids {
        collect_matches(nodes, k, sel, out);
    }
}

fn matches(n: &Node, sel: &str) -> bool {
    let sel = sel.trim();
    if sel.is_empty() || sel == "*" {
        return true;
    }
    if let Some(rest) = sel.strip_prefix("name=") {
        return n.name == rest.trim_matches('"');
    }
    // role[name='x'] / role[name="x"] / role
    if let Some(open) = sel.find('[') {
        let role = &sel[..open];
        let inner = sel[open + 1..].trim_end_matches(']');
        if role != "*" && role != n.role {
            return false;
        }
        if let Some(v) = inner.strip_prefix("name=") {
            return n.name == v.trim().trim_matches('"').trim_matches('\'');
        }
        return true;
    }
    n.role == sel
}
