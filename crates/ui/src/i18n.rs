//! Desktop UI localization.
//!
//! English is the source language in code. Simplified Chinese is selected when
//! the OS locale is Chinese, or when the user pins it under Settings → Appearance.
//! Tests default to English so host locale cannot flip assertions.

use std::cell::Cell;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use gpui::App;
use serde::{Deserialize, Serialize};

use crate::settings::UiSettings;

const MODE_SYSTEM: u8 = 0;
const MODE_EN: u8 = 1;
const MODE_ZH: u8 = 2;

static MODE: AtomicU8 = AtomicU8::new(MODE_SYSTEM);
static SYSTEM_ZH: AtomicBool = AtomicBool::new(false);
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Persisted language preference. Defaults to following the OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LanguageMode {
    #[default]
    System,
    English,
    Chinese,
}

impl LanguageMode {
    pub const ALL: [Self; 3] = [Self::System, Self::English, Self::Chinese];

    pub fn label(self) -> &'static str {
        match self {
            Self::System => t("System"),
            Self::English => "English",
            Self::Chinese => "简体中文",
        }
    }

    fn as_u8(self) -> u8 {
        match self {
            Self::System => MODE_SYSTEM,
            Self::English => MODE_EN,
            Self::Chinese => MODE_ZH,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            MODE_EN => Self::English,
            MODE_ZH => Self::Chinese,
            _ => Self::System,
        }
    }
}

/// Resolved UI locale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locale {
    En,
    Zh,
}

#[cfg(test)]
thread_local! {
    static TEST_LOCALE: Cell<Option<Locale>> = const { Cell::new(Some(Locale::En)) };
}

/// Install the persisted preference. Call once at boot, before the first paint.
pub fn init(mode: LanguageMode, data_dir: PathBuf) {
    let _ = DATA_DIR.set(data_dir);
    SYSTEM_ZH.store(detect_system_zh(), Ordering::Relaxed);
    MODE.store(mode.as_u8(), Ordering::Relaxed);
}

pub fn mode() -> LanguageMode {
    LanguageMode::from_u8(MODE.load(Ordering::Relaxed))
}

/// Pin a language, persist it, rebuild native menus, and repaint.
pub fn set_mode(mode: LanguageMode, cx: &mut App) {
    if mode.as_u8() == MODE.load(Ordering::Relaxed) {
        return;
    }
    MODE.store(mode.as_u8(), Ordering::Relaxed);
    persist(mode);
    cx.set_menus(crate::app_menus::app_menus());
    cx.refresh_windows();
}

fn persist(mode: LanguageMode) {
    let Some(dir) = DATA_DIR.get() else {
        return;
    };
    let mut settings = UiSettings::load(dir);
    settings.language = mode;
    if let Err(err) = settings.save(dir) {
        tracing::warn!(error = %err, "could not persist language");
    }
}

pub fn is_zh() -> bool {
    locale() == Locale::Zh
}

pub fn locale() -> Locale {
    #[cfg(test)]
    {
        if let Some(loc) = TEST_LOCALE.with(Cell::get) {
            return loc;
        }
    }
    match MODE.load(Ordering::Relaxed) {
        MODE_EN => Locale::En,
        MODE_ZH => Locale::Zh,
        _ => {
            if SYSTEM_ZH.load(Ordering::Relaxed) {
                Locale::Zh
            } else {
                Locale::En
            }
        }
    }
}

#[cfg(test)]
pub fn with_locale<T>(loc: Locale, f: impl FnOnce() -> T) -> T {
    TEST_LOCALE.with(|c| {
        let prev = c.get();
        c.set(Some(loc));
        let result = f();
        c.set(prev);
        result
    })
}

/// Translate a static English UI string. Unknown keys stay English.
pub fn t(en: &'static str) -> &'static str {
    if !is_zh() {
        en
    } else {
        lookup_zh(en).unwrap_or(en)
    }
}

/// Pick between two literals when the same English word has two Chinese senses.
pub fn tr(en: &'static str, zh: &'static str) -> &'static str {
    if is_zh() { zh } else { en }
}

/// Translate a runtime English string (engine errors, catalog labels, …).
pub fn t_str(en: &str) -> String {
    if !is_zh() {
        en.to_string()
    } else {
        lookup_zh(en).unwrap_or(en).to_string()
    }
}

/// Template used by [`tf`]: English when the UI is English, Chinese otherwise.
pub fn lookup(en: &str) -> &str {
    if !is_zh() {
        en
    } else {
        lookup_zh(en).unwrap_or(en)
    }
}

macro_rules! tf {
    ($en:literal) => {
        $crate::i18n::lookup($en).to_string()
    };
    ($en:literal, $($key:ident = $val:expr),+ $(,)?) => {{
        let mut s = $crate::i18n::lookup($en).to_string();
        $(
            s = s.replace(concat!("{", stringify!($key), "}"), &$val.to_string());
        )+
        s
    }};
}
pub(crate) use tf;

pub fn detect_system_preview_zh() -> bool {
    detect_system_zh()
}

fn detect_system_zh() -> bool {
    if let Ok(v) = std::env::var("ZERON_LANG") {
        let v = v.to_ascii_lowercase();
        if v == "zh" || v.starts_with("zh_") || v.starts_with("zh-") || v == "cn" {
            return true;
        }
        if v == "en" || v.starts_with("en_") || v.starts_with("en-") {
            return false;
        }
    }
    if macos_prefers_zh() {
        return true;
    }
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.to_ascii_lowercase();
            if v.starts_with("zh") {
                return true;
            }
        }
    }
    false
}

#[cfg(target_os = "macos")]
fn macos_prefers_zh() -> bool {
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};
    use std::ffi::CStr;
    unsafe {
        let langs: *mut Object = msg_send![class!(NSLocale), preferredLanguages];
        if langs.is_null() {
            return false;
        }
        let n: usize = msg_send![langs, count];
        if n == 0 {
            return false;
        }
        let first: *mut Object = msg_send![langs, objectAtIndex: 0usize];
        if first.is_null() {
            return false;
        }
        let ptr: *const i8 = msg_send![first, UTF8String];
        if ptr.is_null() {
            return false;
        }
        let tag = CStr::from_ptr(ptr).to_string_lossy();
        tag.to_ascii_lowercase().starts_with("zh")
    }
}

#[cfg(not(target_os = "macos"))]
fn macos_prefers_zh() -> bool {
    false
}

fn lookup_zh(en: &str) -> Option<&'static str> {
    Some(match en {
        "Zeron is using a background daemon. Stop it and quit Zeron, then reopen to start the synced workspace. Existing local sessions stay on this device and will not be uploaded." => {
            "Zeron 正在使用后台守护进程。请停止它并退出 Zeron，然后重新打开以启动同步工作区。现有本地会话保留在本设备上，不会上传。"
        }
        "Zeron removed your credentials but could not finish closing the previous synced workspace. Retry before continuing in local mode." => {
            "Zeron 已移除你的凭据，但未能完成关闭之前的同步工作区。在继续本地模式前请重试。"
        }
        "Quit and reopen Zeron to start the synced workspace. Existing local sessions stay on this device and will not be uploaded." => {
            "退出并重新打开 Zeron 以启动同步工作区。现有本地会话保留在本设备上，不会上传。"
        }
        "You're signed in as {email}. Bring {phrase} from this device into your synced workspace, or start it fresh." => {
            "你已以 {email} 身份登录。将此设备上的 {phrase} 带入同步工作区，或全新开始。"
        }
        "Finish signing in in your browser. Zeron will keep using this local workspace until you quit and reopen." => {
            "请在浏览器中完成登录。在退出并重新打开 Zeron 之前，会继续使用此本地工作区。"
        }
        "Could not stop the remote engine: {err}. Run `zeron daemon stop`, then quit and reopen Zeron." => {
            "无法停止远程引擎：{err}。请运行 `zeron daemon stop`，然后退出并重新打开 Zeron。"
        }
        "Removing “{name}” permanently deletes its {count} sessions on {device}. This can’t be undone." => {
            "移除“{name}”将永久删除其 {device} 上的 {count} 个会话。此操作无法撤销。"
        }
        "Zeron will remove your credentials, close the synced workspace, and continue in local mode." => {
            "Zeron 将移除你的凭据，关闭同步工作区，并继续以本地模式运行。"
        }
        "Hidden from the sidebar, never deleted. Unarchiving puts a session back on its device." => {
            "从侧边栏隐藏，不会删除。取消归档会让会话回到其所在设备。"
        }
        "Removing “{name}” permanently deletes its 1 session on {device}. This can’t be undone." => {
            "移除“{name}”将永久删除其 {device} 上的 1 个会话。此操作无法撤销。"
        }
        "This chat's device is running an older Zeron — update it to view branch and turn diffs" => {
            "此会话所在设备运行的 Zeron 版本较旧 —— 请更新以查看分支和轮次差异"
        }
        "Claude {window} limit reached — the turn was blocked. Try again after it resets." => {
            "Claude 已达到{window}限额，本轮运行被阻止。限额重置后再试。"
        }
        "Bring {phrase} from this device into your synced workspace, or start it fresh." => {
            "将此设备上的 {phrase} 带入同步工作区，或全新开始。"
        }
        "Handing the engine over to your account. Your local sessions come along next." => {
            "正在将引擎移交给你的账户，本地会话随后会一并迁移。"
        }
        "You're signed in as {email}. Zeron can switch to your synced workspace now." => {
            "你已以 {email} 身份登录。Zeron 现在可以切换到你的同步工作区。"
        }
        "agent does not advertise requested model {requested}; available models: {}" => {
            "智能体没有提供所请求的模型 {requested}；可用：{}"
        }
        "The session's device runs an older zeron — update it to search its files" => {
            "会话所在设备的 Zeron 版本过旧，请更新后再搜索文件"
        }
        "Anything already imported is kept; retrying only copies what's missing." => {
            "已导入的内容会保留；重试只会补齐缺失的部分。"
        }
        "Removing the partial sign-in before returning to your local workspace." => {
            "正在移除未完成的登录，然后返回本地工作区。"
        }
        "The session's device runs an older zeron — update it to list commands" => {
            "会话所在设备的 Zeron 版本过旧，请更新后再加载命令"
        }
        "Type your own answer, or leave this blank to use the selected option" => {
            "输入你的回答，留空则使用所选选项"
        }
        "Claude usage limit reached — try again after the limit resets." => {
            "Claude 已达到使用限额——限额重置后再试。"
        }
        "Removing account credentials and closing the synced workspace." => {
            "正在移除账户凭据并关闭同步工作区。"
        }
        "Anthropic's coding agent, driven through the Claude Code CLI." => {
            "Anthropic 的编程智能体，通过 Claude Code CLI 驱动。"
        }
        "Couldn't upload the attachment — the device may be offline." => {
            "附件上传失败，会话所在设备可能已离线。"
        }
        "Cursor's coding agent, driven through the cursor-agent CLI." => {
            "Cursor 的编程智能体，通过 cursor-agent CLI 驱动。"
        }
        "Diff truncated — showing first {max_lines} of {total} lines" => {
            "差异已截断——显示前 {max_lines} / {total} 行"
        }
        "Billing error — check your Claude plan or payment method." => {
            "Claude 账单错误——请检查套餐或付款方式。"
        }
        "The synced workspace did not come up — restart to finish." => {
            "同步工作区未能启动——重启以完成。"
        }
        "Manage device names and inspect synced device metadata." => {
            "管理设备名称并查看已同步的设备元数据。"
        }
        "Chime when a run finishes or an agent asks a question." => {
            "运行完成或智能体提问时发出提示音。"
        }
        "Follow the language of this device. Currently {lang}." => "跟随本机语言——当前为{lang}。",
        "Manage device details stored in this local workspace." => {
            "管理存储在此本地工作区中的设备详情。"
        }
        "Offline — messages will send when you're back online." => "离线 —— 连上后会自动发送。",
        "OpenAI's coding agent, driven through the Codex CLI." => {
            "OpenAI 的编程智能体，通过 Codex CLI 驱动。"
        }
        "the daemon did not finish stopping within {secs} seconds" => {
            "守护进程未在 {secs} 秒内停止"
        }
        "Claude is overloaded right now — try again shortly." => {
            "Claude 当前负载过高——请稍后再试。"
        }
        "Open a blank session canvas to start a new session." => "打开空白会话画布以开始新会话。",
        "Right-click a session in the sidebar to archive it." => "在侧边栏右键点击会话即可归档。",
        "This organization isn't allowed to use Claude here." => "当前组织无权使用 Claude。",
        "Show or hide the terminal for the current session." => "显示或隐藏当前会话的终端。",
        "Authentication failed — sign in to Claude again." => "Claude 认证失败——请重新登录。",
        "Messages will send once the connection recovers." => "连接恢复后会自动发送。",
        "OpenCode model discovery did not complete within" => "模型发现未在",
        "The run exhausted its structured-output retries." => "运行已耗尽结构化输出重试次数。",
        "agent rejected model switch to {model}: {error}" => {
            "智能体拒绝切换到模型 {model}：{error}"
        }
        "Could not reach the zeron engine on port 27901" => "无法连接 27901 端口的 zeron 引擎",
        "Show or hide sessions and settings navigation." => "显示或隐藏会话与设置导航。",
        "Zeron can switch to your synced workspace now." => "Zeron 现在可以切换到你的同步工作区。",
        "{} model discovery did not complete within {}s" => "{} 模型发现未在 {} 秒内完成",
        "A project is a folder on one of your devices." => "项目是你某台设备上的一个文件夹。",
        "Follow the language of your operating system." => "跟随操作系统的语言。",
        "Show or hide changes for the current session." => "显示或隐藏当前会话的更改。",
        "Type your own answer, or pick an option above" => "输入你的回答，或选择上面的选项",
        "Run interrupted by engine restart — resuming" => "运行被引擎重启中断 —— 正在恢复",
        "No turn recorded yet — send a message first" => "尚未记录到轮次 —— 请先发送一条消息",
        "The import stream ended before it finished." => "导入流提前结束。",
        "Nous Research's Hermes Agent (hermes CLI)." => {
            "Nous Research 的 Hermes 智能体（hermes CLI）。"
        }
        "{count} imported, {errors} failures — first: {first}" => {
            "已导入 {count} 个，{errors} 个失败——首个：{first}"
        }
        "Couldn't load full {what} — tap to retry" => "无法加载完整{what}——点击重试",
        "Handing the engine over to your account." => "正在将引擎移交给你的账号。",
        "No starred models yet — hit a row's star" => "还没有收藏的模型 —— 点击行的星标即可收藏",
        "Or continue in a workspace you belong to" => "或继续使用你所属的工作区",
        "The reply hit the maximum output length." => "回复已达到最大输出长度。",
        "The run hit the maximum number of turns." => "运行已达到最大轮数。",
        "{display_name} is not a supported image." => "{display_name} 不是受支持的图片格式。",
        "{display_name} is too large (24 MB max)." => "{display_name} 过大（最大 24 MB）。",
        "Choose what to show in the right panel." => "选择右侧面板显示的内容。",
        "Manage device names for this workspace." => "管理此工作区的设备名称。",
        "Showing folders from {device_name} only" => "仅显示来自 {device_name} 的文件夹",
        "Claude had a server error — try again." => "Claude 服务端错误——请重试。",
        "Couldn't stage the attachment locally." => "无法在本地暂存附件。",
        "Claude returned an unspecified error." => "Claude 返回了未说明原因的错误。",
        "SST's opencode agent (opencode CLI)." => "SST 的 opencode 智能体（opencode CLI）。",
        "The request was rejected as invalid." => "请求无效，已被 Claude 拒绝。",
        "Couldn't load this agent's commands" => "无法加载此智能体的命令",
        "Switching to your synced workspace…" => "正在切换到你的同步工作区…",
        "The agent process is likely wedged." => "智能体进程可能卡住了。",
        "The selected model isn't available." => "所选模型不可用。",
        "The session's device is unreachable" => "无法连接会话所在设备",
        "Diff stream interrupted — retrying" => "差异流中断——正在重试",
        "xAI's Grok Build agent (grok CLI)." => "xAI 的 Grok Build 智能体（grok CLI）。",
        "Run interrupted by engine restart" => "运行被引擎重启中断",
        "{display_name} could not be read." => "{display_name} 无法读取。",
        "Binary file — contents not shown" => "二进制文件——内容不显示",
        "Queued — will send automatically" => "已排队 —— 将自动发送",
        "Select a chat to open a terminal" => "选择会话以打开终端",
        "This agent has no slash commands" => "此智能体没有可用的斜杠命令",
        "Could not cancel sign-in: {err}" => "无法取消登录：{err}",
        "Importing session {n} of {total}" => "正在导入会话 {n} / {total}",
        "Ran 3 commands · edited 2 files" => "运行了 3 个命令 · 修改了 2 个文件",
        "Showing {shown} of {total} refs" => "显示 {shown} / {total} 个引用",
        "Update ready — restart to apply" => "更新就绪——重启生效",
        "{count} imported, 1 failure: {first}" => "已导入 {count} 个，1 个失败：{first}",
        "Not delivered — click to retry" => "未送达 —— 点击重试",
        "Read 1 file · searched 2 times" => "读取了 1 个文件 · 搜索了 2 次",
        "The pi coding agent (pi CLI)." => "pi 编程智能体（pi CLI）。",
        "{combo} is already assigned to {owner}." => "{combo} 已分配给 {owner}。",
        "Add a project to get started" => "添加项目以开始",
        "Install the {cli} CLI to enable" => "安装 {cli} CLI 后才能启用",
        "{cli} CLI not installed — turn it off or install it" => "{cli} CLI 未安装——请关闭或安装",
        "Login failed to start: {err}" => "登录未能开始：{err}",
        "Paste the authorization code" => "粘贴授权码",
        "Poll failed: malformed reply" => "轮询失败：回复格式不对",
        "The run ended with an error." => "运行因错误结束。",
        "The run hit its cost budget." => "运行已达到费用预算。",
        "the 1 session and 2 projects" => "1 个会话和 2 个项目",
        "Looking for local sessions…" => "正在查找本地会话…",
        "No projects on this device." => "此设备上没有项目。",
        "harness protocol error: {0}" => "智能体协议错误：{0}",
        "{count} Uncommitted changes" => "{count} 个未提交的更改",
        "Remote branch: origin/main" => "远程分支：origin/main",
        "See the attached image(s)." => "查看所附图片。",
        "2 Changed files this turn" => "本轮 2 个文件发生更改",
        "Local development runtime" => "本地开发运行时",
        "Offline — sends are saved" => "离线 —— 发送已保存",
        "Shortcuts must be unique." => "快捷键必须唯一。",
        "Ran 1 command · 1 failed" => "运行了 1 个命令 · 1 个失败",
        "Sign out failed: {error}" => "退出登录失败：{error}",
        "Sync ready after restart" => "重启后同步就绪",
        "Update failed: {message}" => "更新失败：{message}",
        "Waiting for the browser…" => "等待浏览器完成登录…",
        "Authentication disabled" => "已禁用认证",
        "Bringing your work over" => "正在迁移你的数据",
        "Credentials unavailable" => "凭据暂不可用",
        "Press Escape to cancel." => "按 Esc 取消。",
        "Reopen the sign-in page" => "重新打开登录页面",
        "Unarchive failed: {err}" => "取消归档失败：{err}",
        "Use a different account" => "使用其他账号",
        "did not finish stopping" => "守护进程未在",
        "1 Changed file vs main" => "1 个文件发生更改，对比 main",
        "Enter a workspace name" => "请输入工作区名称",
        "No repository selected" => "未选择仓库",
        "No uncommitted changes" => "没有未提交的更改",
        "Scripted test harness." => "脚本化测试智能体。",
        "Sync setup in progress" => "同步设置进行中",
        "0 Uncommitted changes" => "0 个未提交的更改",
        "2 Uncommitted changes" => "2 个未提交的更改",
        "4 Uncommitted changes" => "4 个未提交的更改",
        "Awaiting your answer…" => "等待你的回答…",
        "Canceling sync setup…" => "正在取消同步设置…",
        "Claude error: {other}" => "Claude 错误：{other}",
        "Create your workspace" => "创建你的工作区",
        "Default (recommended)" => "默认（推荐）",
        "Desktop notifications" => "桌面通知",
        "Drop images to attach" => "拖入图片以附加",
        "Fetch failed: {error}" => "拉取失败：{error}",
        "No changes vs develop" => "与 develop 相比没有更改",
        "No devices registered" => "尚未注册设备",
        "Show full {what} ({size})" => "显示完整{what} ({size})",
        "Show {remaining} more" => "再显示 {remaining} 个",
        "Sign in failed: {err}" => "登录失败：{err}",
        "Stored on this device" => "存储在本设备",
        "Waiting on your input" => "等待你的输入",
        "1 Uncommitted change" => "1 个未提交的更改",
        "Engine not connected" => "引擎未连接",
        "Import didn't finish" => "导入未完成",
        "Loading full {what}…" => "正在加载完整{what}…",
        "No changes this turn" => "本轮没有更改",
        "No changes vs {base}" => "与 {base} 相比没有更改",
        "No matching branches" => "没有匹配的分支",
        "No matching commands" => "没有匹配的命令",
        "Rename failed: {err}" => "重命名失败：{err}",
        "Stop daemon and quit" => "停止守护进程并退出",
        "Sync needs a restart" => "同步需要重启",
        "Toggle right sidebar" => "切换右侧栏",
        "{bucket_len} prompts" => "{bucket_len} 个提示词",
        "No project selected" => "未选择项目",
        "Renamed from {from}" => "重命名自 {from}",
        "Toggle left sidebar" => "切换左侧栏",
        "Zeron uses English." => "Zeron 使用英文。",
        "whatever the system" => "无论系统设置如何",
        "{n} attached images" => "{n} 张已附加图片",
        "Add Claude account" => "添加 Claude 账号",
        "Appearance: System" => "外观：跟随系统",
        "Archived ({total})" => "已归档（{total}）",
        "File search failed" => "文件搜索失败",
        "Keyboard shortcuts" => "键盘快捷键",
        "Mode changed to {mode}" => "模式更改为 {mode}",
        "No files available" => "暂无可用文件",
        "No projects match." => "没有匹配的项目。",
        "Open browser again" => "重新打开浏览器",
        "Poll failed: {err}" => "轮询失败：{err}",
        "Stop failed: {err}" => "停止失败：{err}",
        "first 6 of 8 lines" => "显示前 6 / 8 行",
        "%b %-d, %-I:%M %p" => "%-m月%-d日 %H:%M",
        "2 attached images" => "2 张已附加图片",
        "Add Codex account" => "添加 Codex 账号",
        "Appearance: Light" => "外观：浅色",
        "Archived sessions" => "已归档会话",
        "Cancel sync setup" => "取消同步设置",
        "Finish sync setup" => "完成同步设置",
        "No branch changes" => "没有分支更改",
        "No devices match." => "没有匹配的设备。",
        "No matching files" => "没有匹配的文件",
        "Standard · Normal" => "标准 · 正常",
        "Terminal {tab_no}" => "终端 {tab_no}",
        "Usage unavailable" => "用量暂不可用",
        "Appearance: Dark" => "外观：深色",
        "Current checkout" => "当前检出",
        "Current worktree" => "当前工作树",
        "High · 1M · Fast" => "高 · 1M · 快速",
        "Loading history…" => "正在加载历史…",
        "No commits found" => "没有找到提交",
        "No folders match" => "没有匹配的文件夹",
        "Nothing archived" => "没有已归档内容",
        "Partial snapshot" => "部分快照",
        "Restore defaults" => "恢复默认",
        "Retry local mode" => "重试本地模式",
        "Search projects…" => "搜索项目…",
        "Send failed: {e}" => "发送失败：{e}",
        "Show full {what}" => "显示完整{what}",
        "Stopping engine…" => "正在停止引擎…",
        "Untitled session" => "未命名会话",
        "3 Changed files" => "3 个文件发生更改",
        "Log in to Zeron" => "登录 Zeron",
        "No folders here" => "这里没有文件夹",
        "No models found" => "未找到模型",
        "No sessions yet" => "还没有会话",
        "Remove project?" => "移除项目？",
        "Search devices…" => "搜索设备…",
        "Search folders…" => "搜索文件夹…",
        "Toggle terminal" => "切换终端",
        "Unknown account" => "未知账号",
        "currently light" => "当前为浅色",
        "did not respond" => "完全没有响应",
        "local workspace" => "本地工作区",
        "weekly (Sonnet)" => "每周（Sonnet）",
        "Attached image" => "已附加图片",
        "Branch changes" => "分支更改",
        "Click to retry" => "点击重试",
        "Connect Cursor" => "连接 Cursor",
        "Context Window" => "上下文窗口",
        "Context window" => "上下文窗口",
        "Jul 1, 3:45 PM" => "7月1日 15:45",
        "Local checkout" => "当前工作区",
        "No refs found." => "未找到引用。",
        "Open a surface" => "打开面板",
        "Rename project" => "重命名项目",
        "Rename session" => "重命名会话",
        "Search models…" => "搜索模型…",
        "Unknown device" => "未知设备",
        "Workspace name" => "工作区名称",
        "currently dark" => "当前为深色",
        "the 2 sessions" => "2 个会话",
        "Add a project" => "添加项目",
        "Bring my work" => "迁移我的数据",
        "Edited 1 file" => "修改了 1 个文件",
        "Not signed in" => "未登录",
        "Notifications" => "通知",
        "Reconnecting…" => "正在重连…",
        "Remote branch" => "远程分支",
        "Rename device" => "重命名设备",
        "Session title" => "会话标题",
        "Sync is ready" => "同步已就绪",
        "the 1 project" => "1 个项目",
        "unknown error" => "未知错误",
        "weekly (Opus)" => "每周（Opus）",
        "(no subject)" => "（无主题）",
        "All projects" => "全部项目",
        "Branch: main" => "分支：main",
        "Close Window" => "关闭窗口",
        "Deleted file" => "已删除文件",
        "Do anything…" => "随便做点什么…",
        "Empty commit" => "空提交",
        "Last seen {when}" => "上次出现 {when}",
        "Login failed" => "登录失败",
        "New project…" => "新建项目…",
        "New worktree" => "新建工作树",
        "Project name" => "项目名称",
        "Retry import" => "重试导入",
        "Run finished" => "运行完成",
        "Search refs…" => "搜索引用…",
        "Service Tier" => "速度档位",
        "Signing out…" => "正在退出登录…",
        "Tag: v0.1.52" => "标签：v0.1.52",
        "Unarchiving…" => "正在取消归档…",
        "Working tree" => "工作区",
        "this project" => "此项目",
        "About Zeron" => "关于 Zeron",
        "Add account" => "添加账号",
        "Development" => "开发模式",
        "Device name" => "设备名称",
        "Enable sync" => "启用同步",
        "From {name}" => "来自 {name}",
        "Hide Others" => "隐藏其他",
        "Key expired" => "密钥已过期",
        "Key expires" => "密钥",
        "Latest turn" => "最新一轮",
        "New session" => "新会话",
        "No branches" => "没有分支",
        "Press keys…" => "按下按键…",
        "Start fresh" => "重新开始",
        "This device" => "本设备",
        "1 imported" => "已导入 1 个",
        "3 failures" => "3 个失败",
        "Appearance" => "外观",
        "How zeron picks between light and dark. This setting stays on this device." => {
            "Zeron 如何在浅色和深色之间选择。此设置仅保存在本设备上。"
        }
        "Hide Zeron" => "隐藏 Zeron",
        "Local only" => "仅本地",
        "No project" => "无项目",
        "Quit Zeron" => "退出 Zeron",
        "Run failed" => "运行失败",
        "Select All" => "全选",
        "Select ref" => "选择引用",
        "Signed out" => "已退出登录",
        "Switch now" => "立即切换",
        "Switching…" => "切换中…",
        "Verifying…" => "验证中…",
        "foo in src" => "foo 于 src",
        "its device" => "其设备",
        "never seen" => "从未出现",
        "Creating…" => "正在创建…",
        "Fast Mode" => "快速模式",
        "Fetch all" => "拉取全部",
        "Fetching…" => "正在拉取…",
        "Load more" => "加载更多",
        "Reasoning" => "推理",
        "Shortcuts" => "快捷键",
        "Sign out?" => "退出登录？",
        "Unarchive" => "取消归档",
        "following" => "跟随",
        "workspace" => "工作区",
        "1/2 done" => "1/2 已完成",
        "Accounts" => "账号",
        "Added {when}" => "添加于 {when}",
        "Archived" => "已归档",
        "Complete" => "完成",
        "Continue" => "继续",
        "Flagship" => "旗舰",
        "Language" => "语言",
        "Loading…" => "正在加载…",
        "Minimize" => "最小化",
        "Navigate" => "进入",
        "New file" => "新文件",
        "Question" => "提问",
        "Sending…" => "正在发送…",
        "Services" => "服务",
        "Settings" => "设置",
        "Show All" => "全部显示",
        "Sign out" => "退出登录",
        "Standard" => "标准",
        "Terminal" => "终端",
        "Thinking" => "思考模式",
        "Worktree" => "工作树",
        "just now" => "刚刚",
        "provider" => "供应商",
        "{pct}% used" => "已用 {pct}%",
        "Adding…" => "正在添加…",
        "Archive" => "归档",
        "Default" => "默认",
        "Delete…" => "删除…",
        "Devices" => "设备",
        "English" => "English",
        "History" => "历史",
        "Minimal" => "最低",
        "Remove…" => "移除…",
        "Rename…" => "重命名…",
        "Search…" => "搜索…",
        "Sending" => "发送中",
        "Session" => "本轮",
        "Unknown" => "未知",
        "Working" => "工作中",
        "overage" => "超额",
        "{n}d ago" => "{n} 天前",
        "{n}h ago" => "{n} 小时前",
        "{n}m ago" => "{n} 分钟前",
        "2d ago" => "2 天前",
        "3h ago" => "3 小时前",
        "5-hour" => "5 小时",
        "5m ago" => "5 分钟前",
        "Active" => "当前",
        "Agents" => "智能体",
        "Author" => "作者",
        "Binary" => "二进制",
        "Branch" => "分支",
        "Cancel" => "取消",
        "Closed" => "已关闭",
        "Commit" => "提交",
        "Copied" => "已复制",
        "Create" => "创建",
        "Delete" => "删除",
        "Failed" => "失败",
        "Log in" => "登录",
        "Medium" => "中",
        "Merged" => "已合并",
        "No ref" => "无引用",
        "Normal" => "正常",
        "Queued" => "已排队",
        "Remove" => "移除",
        "Rename" => "重命名",
        "Search" => "搜索",
        "Sounds" => "声音",
        "Switch" => "切换",
        "System" => "系统",
        "Traits" => "参数",
        "Window" => "窗口",
        "X-High" => "极高",
        "output" => "输出",
        "weekly" => "每周",
        "Agent" => "智能体",
        "Close" => "关闭",
        "Enter" => "回车",
        "Error" => "错误",
        "Fetch" => "抓取",
        "Input" => "待输入",
        "Later" => "稍后",
        "Light" => "浅色",
        "Local" => "本机",
        "Paste" => "粘贴",
        "Patch" => "补丁",
        "Reset" => "重置",
        "Retry" => "重试",
        "Shell" => "命令行",
        "Speed" => "速度",
        "Theme" => "主题",
        "Write" => "写入",
        "items" => "模型发现超时",
        "light" => "浅色",
        "usage" => "使用量",
        "Back" => "返回",
        "Copy" => "复制",
        "Dark" => "深色",
        "Date" => "日期",
        "Done" => "完成",
        "Edit" => "编辑",
        "Fast" => "快速",
        "Glob" => "匹配",
        "High" => "高",
        "Open" => "开放",
        "Read" => "读取",
        "Redo" => "重做",
        "Text" => "文本",
        "Todo" => "待办",
        "Tool" => "工具",
        "Undo" => "撤销",
        "View" => "视图",
        "Week" => "本周",
        "Zoom" => "缩放",
        "dark" => "深色",
        "diff" => "差异",
        "Cut" => "剪切",
        "Low" => "低",
        "Off" => "关闭",
        "Run" => "运行",
        "Tag" => "标签",
        "Web" => "网页",
        "You" => "本机",
        "now" => "刚刚",
        "On" => "开启",
        "Up" => "上一级",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tests_default_to_english() {
        assert_eq!(t("Appearance"), "Appearance");
        assert_eq!(t_str("No project"), "No project");
    }

    #[test]
    fn chinese_lookup_and_passthrough() {
        with_locale(Locale::Zh, || {
            assert_eq!(t("Appearance"), "外观");
            assert_eq!(t("Accounts"), "账号");
            assert_eq!(t("___missing_key___"), "___missing_key___");
            assert_eq!(tr("Open", "打开"), "打开");
        });
        assert_eq!(tr("Open", "打开"), "Open");
    }

    #[test]
    fn tf_named_and_positional() {
        with_locale(Locale::Zh, || {
            let n = 3;
            assert_eq!(tf!("{n} attached images", n = n), "3 张已附加图片");
            assert_eq!(tf!("{pct}% used", pct = 12), "已用 12%");
        });
        let n = 3;
        assert_eq!(tf!("{n} attached images", n = n), "3 attached images");
        assert_eq!(tf!("{pct}% used", pct = 12), "12% used");
    }

    #[test]
    fn relative_now_localizes() {
        assert_eq!(t_str("now"), "now");
        with_locale(Locale::Zh, || {
            assert_eq!(t_str("now"), "刚刚");
        });
    }

    #[test]
    fn language_mode_round_trips_in_settings() {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = UiSettings::default();
        assert_eq!(settings.language, LanguageMode::System);
        settings.language = LanguageMode::Chinese;
        settings.save(dir.path()).unwrap();
        let loaded = UiSettings::load(dir.path());
        assert_eq!(loaded.language, LanguageMode::Chinese);
    }
}
