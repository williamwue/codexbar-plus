# CodexBar → Windows 移植分析（基于 upstream 源码实测）

上游快照：`upstream-ref/`（`git clone --depth 1 https://github.com/steipete/CodexBar`，76 MB，2771 文件，MIT）。
所有结论均来自该快照实测，标注 `path:line`；推测标 `[推测]`。

---

## 1. 上游是什么

macOS 14+ 菜单栏工具：把 69 家 AI 编码服务商的**配额窗口 / 重置倒计时 / credit 余额 / 花费**塞进菜单栏，动态绘制条形"油量表"图标。核心卖点是"不用登录"——复用系统里已有的凭据（OAuth token 文件、浏览器 cookie、CLI 配置、API key）。

### 代码体量（实测 `find | wc -l`）

| Target | 文件 | 行数 | 性质 |
|---|---:|---:|---|
| `Sources/CodexBarCore` | 650 | 177,219 | provider 引擎、HTTP、成本库、配置、插件宿主 —— **已跨平台（Linux CLI 在售）** |
| `Sources/CodexBar` | 438 | 99,995 | AppKit/SwiftUI 菜单栏 App —— **纯 mac，必须重写** |
| `Sources/CodexBarCLI` | 40 | 16,000 | `codexbar` 命令行 —— 跨平台 |
| `Sources/CodexBarWidget` | 6 | 2,837 | WidgetKit —— mac/iOS only |
| `AdaptiveRefreshCore` + `AdaptiveReplayKit` | 11 | 1,297 | 纯 Foundation 决策数学 —— 完全可移植 |

关键事实：上游已经把"引擎"和"UI"切开，并且**引擎已经在 Linux（glibc + musl）上发布二进制**（`docs/cli.md:25-30`）。Linux 那条路就是 Windows 的蓝图；Linux 上被排除的能力，正是 Windows 上要补的坑。

---

## 2. Provider 抽象（移植的核心资产）

### 策略协议（`Sources/CodexBarCore/Providers/ProviderFetchPlan.swift:280-286`，原文）

```swift
public protocol ProviderFetchStrategy: Sendable {
    var id: String { get }
    var kind: ProviderFetchKind { get }
    func isAvailable(_ context: ProviderFetchContext) async -> Bool
    func fetch(_ context: ProviderFetchContext) async throws -> ProviderFetchResult
    func shouldFallback(on error: Error, context: ProviderFetchContext) -> Bool
}
```

- `ProviderFetchKind`（`:264-272`）= `cli | web | oauth | apiToken | localProbe | webDashboard`，这就是权威的策略分桶。
- `ProviderDescriptor`（`ProviderDescriptor.swift:309-357`）是值类型描述符：`id / metadata / branding / tokenCost / pace / history / presentation / settingsSection / credentials / config / menuBarMetrics / fetchPlan / cli`。
- 注册表 `ProviderDescriptorRegistry`（`:359-393`）由**代码生成**的封闭清单引导：`Providers/ProviderManifest.swift:1-76`，恰好 69 条。
- 规范数据模型：`UsageFetcher.swift:3-73` 的 `RateWindow`（`usedPercent / windowMinutes / resetsAt / resetDescription / nextRegenPercent / isSyntheticPlaceholder`）+ `:143-176` 的 `UsageSnapshot`（primary/secondary/tertiary + extraRateWindows + cost + identity + confidence），`CreditsModels.swift:45-48` 的 `CreditsSnapshot`。
- 共享 HTTP：`ProviderHTTPClient.swift:6-183` —— `ProviderHTTPTransport` 协议 + 重试策略（408/429/500/502/503/504、超时/连接/DNS 错误、仅幂等方法、指数退避封顶 10s、尊重 `Retry-After`）。

**移植结论**：这一层是纯数据 + 纯 HTTP，任何语言都能 1:1 复刻。它是这次移植真正要抄的东西。

### 69 个 provider 的策略分布（实测正则统计 `kind: ProviderFetchKind = .x`）

| kind | provider 目录数 |
|---|---:|
| `web`（浏览器 cookie） | 30 |
| `apiToken` | 26（另有 16 个走 JS 插件，见下） |
| `cli`（子进程/PTY） | 10 |
| `localProbe`（本地文件/SQLite/localhost） | 6 |
| `oauth` | 6（codex、claude、grok、kimi、vertexai、antigravity） |
| `webDashboard` | 1（codex 网页看板增强） |

### ★ 最重要的发现：上游正在把 provider 迁成 JavaScript 插件

- `Sources/CodexBarCore/Resources/Plugins/` 内已有 **16 个 provider 的 `.js` 实现**（clawrouter、clinepass、crof、deepgram、manus、openai、openrouter、perplexity、poe、qoder、sub2api、synthetic、t3chat、venice、xai、zai），走 `ScriptFetchStrategy`。
- 宿主 API 契约有正式声明：`codexbar-plugin.d.ts`（202 行，`defineProvider()` + `CodexBarPluginContext`：HTTP broker、cookie 导入、设置、失败分类、快照构造）+ `provider-plugin-prelude.js`（230 行）+ 内置 `sucrase-3.35.1.min.js` 做 TS 转译。
- 引擎：macOS 用 JavaScriptCore（`Package.swift:91`），非 mac 用**自带的 QuickJS**（`Sources/CQuickJS`，`Package.swift:57-67`）。
- 上游自己维护了一张移植难度表：`docs/plugin-conversion-matrix.md`，按"下一个阻塞点"给 68 个 provider 分桶：

| 上游分桶 | 数量 | 对 Windows 的含义 |
|---|---:|---|
| `cut-over`（已只剩 JS 实现） | 11 | **实现插件宿主即免费获得** |
| `converted`（JS 版本已并存） | 5 | 同上 |
| `convertible-now`（固定 origin + header 密钥 + GET） | 10 | 一人一天能写几个 |
| `needs-cookie-import` | 19 | 取决于 Windows 浏览器 cookie 方案（最大风险） |
| `needs-files/subprocess/oauth-broker` | 15 | 需要 Windows 路径 + DPAPI + 子进程 |
| `needs-pty/webview/native` | 8 | 最硬：ConPTY、WebView2 抓取、gRPC-web（codex、claude、grok、kiro、jetbrains、antigravity、warp、azureopenai、wayfinder） |

这张表可以直接当作 Windows 版的排期依据：**26 个 provider 走插件宿主几乎零成本，10 个便宜，剩下的按价值挑**。

### 两个旗舰 provider 的完整技术细节（已提取到可直接重写的粒度）

**Codex**
- 凭据：`FileManager.homeDirectoryForCurrentUser/.codex/auth.json`（`CodexOAuthCredentials.swift:111-113`）；旁路 `~/.config/opencode/auth.json`（`:426-428`）。Windows 同构路径 `%USERPROFILE%\.codex\auth.json`。
- 端点（`CodexOAuthUsageFetcher.swift:391-395`）：`https://chatgpt.com/backend-api/` + `/wham/usage`、`/api/codex/usage`、`/wham/rate-limit-reset-credits`。
- 头（`:420-423`）：`Authorization: Bearer <access>`、`User-Agent: CodexBar`、`Accept: application/json`；带账号时加 `ChatGPT-Account-Id`（`:500-504`）；codex 端点加 `OpenAI-Beta: codex-1`（`:545-549`）。
- 窗口字段：`used_percent / reset_at`(unix 秒)`/ limit_window_seconds`（`:213-222`），映射见 `CodexAdditionalRateLimitMapper.swift:78-82`。
- 刷新：`https://auth.openai.com/oauth/token`，client_id `app_EMoamEEZ73f0CkXaXp7hrann`（`CodexTokenRefresher.swift:7-8`）。

**Claude**
- 凭据文件：`~/.claude/.credentials.json`（`ClaudeOAuthCredentials.swift:2939-2943`，支持 `CLAUDE_CONFIG_DIR`）；macOS 另有 Keychain service `Claude Code-credentials`（`:14-16`、`:977-989`）→ **Windows 上 Claude Code 只有文件，反而更简单**。
- 端点（`ClaudeOAuthUsageFetcher.swift:60-64`）：`https://api.anthropic.com` + `/api/oauth/usage`、`/api/oauth/profile`，必须带 `anthropic-beta: oauth-2025-04-20`（`:83-87`）。
- 窗口：`five_hour / seven_day / seven_day_oauth_apps / seven_day_opus / seven_day_sonnet / routines`，字段 `utilization + resets_at`（`:277-299`、`:356-383`）。
- 刷新：`https://platform.claude.com/v1/oauth/token`（`ClaudeOAuthCredentials.swift:27-30`）。
- Web 路径（`ClaudeWebAPIFetcher.swift:134-188`）：`/api/organizations` → org UUID → `/organizations/{id}/usage`、`/prepaid/credits`；**当前实现显式 `notSupportedOnThisPlatform` 非 mac**。

---

## 3. 平台耦合矩阵（Windows 对应物）

| 上游能力 | 位置 | 判定 | Windows 对应 |
|---|---|---|---|
| HTTP/JSON/解析/重试 | `ProviderHTTPClient.swift:6-183` | PORTABLE | 任意 HTTP 客户端 |
| 配置解析 | `Config/CodexBarConfigStore.swift:76-116` — `CODEXBAR_CONFIG_PATH` → `XDG_CONFIG_HOME/codexbar/config.json` → `~/.config/codexbar/config.json` → 旧 `~/.codexbar/config.json` | PORTABLE | 追加 `%APPDATA%\CodexBar\config.json` |
| 配置文件权限 `chmod 0600` | 同上 `:126-130`（仅 mac/Linux） | NEEDS-EQUIV | ACL：仅 `SID(current user)` + `SYSTEM`，禁继承 |
| 密钥存储（Keychain） | `KeychainAccessPreflight.swift:193-204,249-253`；`KeychainCacheStore.swift:17-19,123-125`；`ClaudeOAuthCredentials+SecurityCLIReader.swift:217-225` 调 `/usr/bin/security` | MAC-ONLY | **DPAPI `CryptProtectData`(CurrentUser)** 或 **`CredWriteW`**；两者都不防同用户恶意进程 |
| 浏览器 cookie 提取 | 全部由 mac-only `SweetCookieKit` 承担：`BrowserDetection.swift:2-6`、`BrowserLocalStorageAPI.swift:1-3,97-100`、`BrowserCookieImportOrder.swift:1-10,37-64`（非 mac 是空 shim，`usesKeychainForCookieDecryption` 恒 false） | MAC-ONLY | 见 §5 风险 1 |
| 子进程 | `Host/Process/SubprocessRunner.swift:190-204`（Foundation `Process` + `Pipe`） | NEEDS-EQUIV | `CreateProcessW` + Job Object（保证子树清理） |
| PTY | `Host/PTY/TTYCommandRunner.swift:504-575`（`openpty`/`fcntl`） | NEEDS-EQUIV | **ConPTY `CreatePseudoConsole`**（node-pty / wezterm / windows-rs） |
| 进程树/进程枚举 | `SpawnedProcessGroup.swift:130-149`（`proc_pidinfo` vs `/proc`）、`DarwinProcessEnumerator.swift:58-66`（`proc_listallpids`，用于 agent-aware 刷新） | NEEDS-EQUIV | `CreateToolhelp32Snapshot` / `NtQueryInformationProcess`（命令行需 PEB 读取或 WMI `Win32_Process.CommandLine`） |
| SQLite（成本库、Codex `state_5.sqlite`、Windsurf/Chromium 库） | `CodexThreadCatalogReader.swift:2-5`、`CostUsageStore.swift` | PORTABLE 逻辑 | 需自带 sqlite3（vcpkg / rusqlite bundled / better-sqlite3） |
| JS 插件宿主 | JavaScriptCore(mac) / `CQuickJS`(其他) | PORTABLE | QuickJS(rquickjs) 或 Node/Bun |
| App Group 共享容器 | `AppGroupSupport.swift:86-90,220-237`（`SecStaticCode` team id） | MAC-ONLY | `%LOCALAPPDATA%\CodexBar\` |
| 状态栏 icon 绘制 | `IconRenderer.swift:136-180,760-833`（`NSBezierPath` 圆角条）+ `:1009-1063` 事故角标 | NEEDS-EQUIV | 逻辑照抄；绘制换 Direct2D / Skia / `System.Drawing`，通过 `Shell_NotifyIcon` 换图 |
| 菜单栏布局/条件表达式 | `MenuBarLayout.swift:270-320`（Codable，存 UserDefaults `menuBarLayoutConditionals(V2)`）+ `MenuBarLayoutRenderer.swift:90-290` | PORTABLE 模型 | 模型照抄；渲染重写 |
| 状态项 / 菜单 / 卡片 / 设置窗 | `StatusItemController*.swift`、`PreferencesView.swift`、`MenuCardView.swift`、`SettingsWindowController.swift`（880×620） | MAC-ONLY | 托盘 + 弹出窗（见 §4） |
| 通知 | `AppNotifications.swift`（`UNUserNotificationCenter`） | NEEDS-EQUIV | Windows Toast（AppUserModelID 必需） |
| 撒花动效 | `ScreenConfettiOverlayController.swift`（Vortex + 穿透 NSPanel） | 可选 | 透明置顶 `WS_EX_TRANSPARENT` 层 |
| 全局快捷键 | `KeyboardShortcuts`（`CodexbarApp.swift:470-485`） | NEEDS-EQUIV | `RegisterHotKey` |
| 开机自启 | `LaunchAtLoginManager.swift`（`SMAppService`） | NEEDS-EQUIV | `HKCU\...\Run` 或 StartupTask |
| 自动更新 | Sparkle（`CodexbarApp.swift:180-260`） | NEEDS-EQUIV | **Velopack**（推荐）/ WinSparkle / MSIX |
| iCloud 同步 | `Sync/SyncModels.swift:1-240`（可移植记录 + 规范化 JSON `CanonicalSyncJSON.swift:1-28`，SHA256）+ `CloudSyncEngine.swift`（CloudKit） | 模型 PORTABLE，引擎 MAC-ONLY | v1 直接砍；或后续走文件/自建 |
| WidgetKit（6 个 widget） | `CodexBarWidgetBundle.swift:1-120` | MAC-ONLY | Windows Widgets Board 需打包 provider + adaptive card，**v1 砍掉** |
| 本地化 | `Resources/*.lproj/Localizable.strings` + `.stringsdict`，23 个 locale 目录 | NEEDS-EQUIV | 转 JSON/ICU；`.stringsdict` 复数规则要转换 |

### 已知本地数据源路径（Windows 需逐个映射）

| 来源 | macOS | Windows |
|---|---|---|
| Codex | `CODEX_HOME` / `~/.codex`：`sessions/`、`archived_sessions/`、`state_5.sqlite`、`config.toml`（`CodexLocalDataScope.swift:21-46`、`CodexThreadMetadataReader.swift:182`） | `%USERPROFILE%\.codex\...`（同构） |
| Claude 会话日志 | `~/.claude`（成本扫描解析 `input_tokens / cache_creation_input_tokens / cache_read_input_tokens / output_tokens`，`CostUsageScanner+Claude.swift:168-210`） | `%USERPROFILE%\.claude`（同构） |
| 成本库 | `~/Library/Caches/CodexBar/cost-usage/cost-usage.sqlite`（`CostUsageStore.swift:168-183`） | `%LOCALAPPDATA%\CodexBar\cost-usage\` |
| gcloud（VertexAI） | `~/.config/gcloud` | `%APPDATA%\gcloud`（`credentials.db`） |
| AWS（Bedrock） | `~/.aws` | `%USERPROFILE%\.aws`（同构） |
| Gemini CLI | `~/.gemini/settings.json` | `%USERPROFILE%\.gemini`（同构） |
| Chromium 系 | `~/Library/Application Support/...` | `%LOCALAPPDATA%\Google\Chrome\User Data`、`...\Microsoft\Edge\User Data`、`...\BraveSoftware\Brave-Browser\User Data` |
| Firefox | — | `%APPDATA%\Mozilla\Firefox\Profiles\<p>\cookies.sqlite`（cookie 值明文） |

### 成本/历史库 schema（可直接复刻，`CostUsageStore.swift:662-825`）

表：`meta(key,value)`、`scan_metadata(id=1,payload)`、`files(id,path UNIQUE,inode,mtime_ms,size,parsed_bytes,anchor_indexed_bytes,anchor_window_start,anchor_sha256,scan_state,scan_target_size,scan_complete,session_id,coverage_since_day,coverage_until_day,updated_at_ms)`、`token_snapshots(file_id,event_index,timestamp,timestamp_ms,day,input/cached/output/reasoning,end_offset)`、`usage_rows(file_id,row_index,payload)`、`file_day_aggregates`、`day_aggregates(day,model,…)`、`fork_lineage(file_id,session_id,forked_from_id,fork_timestamp,dependency_key,subagent_state,accounting_state)`、`buffered_lines`、`discovery_state`、`lookback_state`、`accumulators`。
`PRAGMA journal_mode=WAL` + `foreign_keys=ON` + busy timeout（`:823-825`）。预算上限 25,000 行 / 256 MiB（`CostUsageStore+CodexCache.swift:46-47`），保留策略 `CostUsageStore+Retention.swift:20-180`（按天窗口删除，保护 fork 父节点、缓冲行、mtime 活跃文件，迭代到不动点）。

### 自适应刷新算法（`AdaptiveRefreshPolicyCore.swift:40-111`，纯数学，照抄）

优先级从上到下：
1. 低电量 / 热受限 → **30m**
2. 菜单从未打开 → **30m**
3. 菜单最近打开距今 ≤5m → **2m**；≤60m → **5m**；<4h → **15m**；否则 **30m**
4. 5 分钟内检测到编码活动 → 把 >5m 的间隔压到 **5m**

agent-aware 变体本身不做进程检查（`AgentAwarePolicies.swift:9-31` 只做输入投影），进程枚举在平台适配层（`DarwinProcessEnumerator.swift:58-66`）。

### CLI 表面（`CLIEntry.swift:130-340`）

`cards`、`usage`、`guard`、`cost`、`sessions(list|focus)`、`serve`、`dashboard`、`config(validate|dump|providers|enable|disable|set-api-key)`、`hooks(list|enable|disable|test|watch)`、`cache(clear)`、`cookie(refresh)`、`diagnose`、`plugins(list|fetch)`。
输出：`usage` 支持 text/json/**toon**（`CLIUsageCommand.swift:235-265`），其余 text/json；`cards --brief` 是 Provider/Usage/Reset 三列表（`CLICardsBriefRenderer.swift:161-199`）。
Linux 上被排除的：`cookie refresh`（`CLICookieCommand.swift:19-70`）、浏览器 cookie 插件（`CLIPluginsCommand.swift:51-53`）、`sessions focus`（`CLISessionsCommand.swift:44-58`）、需要 web 源的 usage（`CLIUsageCommand.swift:492+`）。**手工 cookie 仍可用** —— 这就是 Windows v1 的保底方案。

---

## 4. 三条实现路线

### 路线 A：把 Swift Core 编到 Windows + 原生托盘外壳
- Swift 6.2 官方支持 Windows（`winget install --id Swift.Toolchain`，需 VS2022 + Win11 SDK）；`Foundation.Process` 有 `#if os(Windows)` 分支走 `CreateProcessW`；`URLSession` 在非 Apple 平台走 libcurl（`FoundationNetworking`）。
- 但必须改造：① `Package.swift:24-26` 硬写 `platforms: [.macOS(.v14)]`；② 依赖 `SweetCookieKit` 官方 manifest 就是 `platforms: [.macOS(.v13)]` + 依赖 macOS Keychain Safe Storage → **硬阻塞，必须替换**；③ `CSQLite3` 只声明 apt/brew provider，Windows 要自己给 sqlite3 module map + 链接；④ `CQuickJS` 用 `_GNU_SOURCE`，MSVC/clang-cl 兼容性待验；⑤ 650 个文件里散布 20+ 处 mac 条件编译需补 Windows 分支。
- 与 UI 的交互：**推荐 CLI + 版本化 JSON stdout**。Swift C ABI DLL 可行但踩坑多（Cascable 的 Windows POC 记录：生成的头 MSVC 无法解析、需 Clang + C++/CLI 双层包装、静态链接崩溃需全改动态库）。
- 收益：69 provider 全量 + 成本库 + 全部解析逻辑一次到手，可跟随上游 rebase。
- 代价：Swift-on-Windows 工具链风险 + 长期维护一个 macOS-first 大仓的 fork。

### 路线 B（推荐）：Rust 引擎 + QuickJS 插件宿主 + Tauri v2 外壳
- 引擎用 Rust 重写 `ProviderFetchStrategy` 那一层（HTTP+JSON，reqwest/serde 很轻），**并用 `rquickjs` 实现上游的 `codexbar-plugin.d.ts` 宿主契约** → 直接跑上游 `Resources/Plugins/*.js`，26 个 provider 近乎免费，且上游每转一个 JS 插件 Windows 就免费多一个。
- Win32 适配全在 Rust（`windows-rs`）：DPAPI/CredWrite、ConPTY、Toolhelp 进程枚举、Shell_NotifyIcon 动态图标、RegisterHotKey、ACL。
- UI 在 WebView2（TS/React）：卡片、图表、23 语言、69 个 provider 设置页用 web 技术做最快，视觉也最容易贴近上游。
- 分发：单 exe + Velopack 增量更新；idle 内存约 10–30 MB。
- 代价：Codex/Claude/Cursor/Copilot 等"硬桶"provider 要手写（本文已把 Codex/Claude 的文件路径、端点、请求头、字段全部提取出来，重写量约 1–2k 行）；成本库 schema 要照抄。

### 路线 C：Electron/Node 全家桶
- 优势：`node-pty`（ConPTY 最成熟）、`better-sqlite3`、生态最快出活；JS 插件可原生执行（连 QuickJS 都不用）。
- 劣势：常驻内存 150 MB+ 对"常驻托盘小工具"不合适；Win32 细节要靠原生模块。

### 建议
**B**。理由：唯一同时满足 ①不赌 Swift-on-Windows ②不重写 69 个 provider ③常驻占用可接受 ④能持续吃上游成果 的方案。JS 插件契约是这次移植的最大杠杆，必须作为一等公民实现。
（本机现状：`cargo 1.96` ✅、`node 22.23` ✅、**无 .NET SDK**（只有 runtime）、无 Swift/MSVC —— 也偏向 B。）

---

## 5. 硬风险

1. **Chrome 127+ App-Bound Encryption（`v20`）—— 19 个 cookie 类 provider 的命门。**
   传统方案（`Local State` 里 `os_crypt.encrypted_key` 经 DPAPI 解出，再 AES-256-GCM 解 `v10` 值）在新版 Chrome 上失效：key 变成 `os_crypt.app_bound_encrypted_key`，值前缀 `v20`，解密需要浏览器 elevation service / `IElevator` broker，实现随版本漂移、可能要提权（Chromium `components/os_crypt/sync/app_bound_encryption_win.cc`）。
   → **v1 策略**：Firefox（`cookies.sqlite` 明文）+ 旧版 `v10` + **手工粘贴 cookie / cURL 导入**（上游 Linux 就是这么活的）作为一等入口；`v20` 列为实验特性，别承诺。
2. **需要 WebView2 抓取的 provider**（codex 网页看板、claude web）本质是在跑浏览器；WebView2 能做，但属于 v2 范畴。
3. **SmartScreen**：未签名 exe 首次运行会被拦。Velopack + 逐步积累 reputation，或买 OV/EV 证书。
4. **WidgetKit / iCloud Sync / `sessions focus`**：Windows 无等价物或成本过高，v1 明确不做。
5. **上游节奏**：仓库 2771 文件、69 provider 仍在加，任何"手写 provider"路线都会持续落后 —— 再次说明插件宿主的必要性。
6. **许可与命名**：上游 MIT，可以合法 fork/移植；建议保留 MIT + 明确署名，产品名避免直接用 `CodexBar` 以免混淆（例：`CodexBar for Windows (unofficial)`）。

---

## 6. 里程碑

| M | 目标 | 验收 |
|---|---|---|
| **M0** 骨架 | Tauri v2 + 托盘 + 动态图标（照抄 `IconRenderer` 条形几何）+ 单 provider 硬编码假数据 | 托盘出现油量表图标，点击弹出卡片 |
| **M1** Codex + Claude | 读 `%USERPROFILE%\.codex\auth.json` / `.claude\.credentials.json`，OAuth token 刷新，调 §2 端点，`RateWindow` 模型，倒计时 | 真账号显示 session/weekly 剩余与重置时间，与 macOS 版数值一致 |
| **M2** 插件宿主 | `rquickjs` + 实现 `codexbar-plugin.d.ts` 全部宿主能力（HTTP broker/设置/失败分类/快照）+ Sucrase TS 转译 | 上游 16 个 `.js` 插件**原样**跑通 |
| **M3** 密钥与配置 | 配置解析（含 `%APPDATA%`）+ ACL 收紧 + DPAPI/CredWrite 存 API key + 设置 UI（provider 开关/key 录入） | `convertible-now` 那 10 个 provider 可用 |
| **M4** 本地源 | 成本库 SQLite（照抄 schema）+ Codex/Claude JSONL 扫描 + `state_5.sqlite` 读取 + 自适应刷新（§ 算法）+ 状态轮询/事故角标 | 今日/30 天花费与 macOS 版对得上 |
| **M5** cookie 与 CLI 源 | Firefox/`v10` cookie 导入 + 手工 cookie/cURL 面板 + `CreateProcessW`/ConPTY 跑 `claude`/`gemini`/`aws` 等 | Cursor 等 web 类 provider 至少走通手工路径 |
| **M6** 产品化 | Toast 通知、全局快捷键、开机自启、Velopack 更新、i18n（含 zh-Hans/zh-Hant）、`codexbar.exe` CLI（`usage`/`cards`/`cost`/`config`） | 安装包可发布，更新可回滚 |

---

## 7. 已落地实现（M0 + M1）

技术栈定稿：**Rust 引擎 + Tauri v2 (WebView2) 托盘外壳**，provider 引擎与 UI 分离。

```
crates/codexbar-core/   引擎：模型、HTTP 重试、路径解析、JWT、DPAPI/ACL、自适应刷新、Codex/Claude provider
crates/codexbar-cli/    codexbar.exe：usage / cards / providers / diagnose
src-tauri/              托盘 App：tiny-skia 绘制托盘表盘、弹出卡片、后台轮询
ui/                     弹出层（原生 HTML/CSS/JS，无构建步骤）
tools/mock-claude.py    Anthropic OAuth 端点 mock，用于无凭据环境下验证请求头与映射
```

对齐上游的部分（均带 `path:line` 注释指向 upstream 出处）：

| 上游 | 本仓实现 |
|---|---|
| `RateWindow` / `UsageSnapshot` / `CreditsSnapshot` | `crates/codexbar-core/src/model.rs`，字段与语义一致（`used_percent` 为“已用”） |
| `ProviderHTTPClient` 重试策略 | `src/http.rs`：408/429/5xx + 仅幂等方法 + 指数退避封顶 10s + `Retry-After` |
| `AdaptiveRefreshPolicyCore` | `src/adaptive.rs`：阈值/延迟/优先级逐条照抄，含时钟回拨与编码活动封顶 |
| `CodexAdditionalRateLimitMapper` | `src/providers/codex.rs`：Spark 双车道 id (`codex-spark` / `codex-spark-weekly`)、slug 规则、≤6h/≥6d 判定 |
| Claude 车道优先级 | `src/providers/claude.rs`：five_hour → seven_day → oauth_apps → sonnet → opus；secondary=weekly；tertiary=sonnet??opus；scoped limits + Routines |
| `IconRenderer` 表盘几何 | `src-tauri/src/icon.rs`：36×36 画布、30px 宽、上 y19/h12、下 y5/h8、胶囊圆角、track 0.28/描边 0.44、单车道走 y14/h16 |
| 最高使用率自动选择 | `src-tauri/src/state.rs`：`select_highest_usage()`，用户手动切换后不再抢占 |
| `chmod 0600` | `src/secure.rs`：`icacls /inheritance:r /grant:r <user>:(F)` |

**修正了上游文档缺失的线上格式**（upstream 无该端点 fixture）：`credits.balance` 实际是字符串、
`additional_rate_limits[].rate_limit` 是带 primary/secondary 的嵌套对象、payload 直接带 `email`、
`rate_limit_reset_credits` 内联计数、`reset_after_seconds` 相对重置时间。抓到的真实响应已脱敏保存为
`crates/codexbar-core/tests/fixtures/codex-wham-usage.json` 并做回归测试。

### 构建与运行

前置：MSVC 生成工具 + Windows SDK（本机已装 BuildTools 17.14 + SDK 10.0.26100，rustc 1.96 MSVC）。

```powershell
cargo test                      # 62 个测试
cargo run -p codexbar-cli -- usage          # 命令行读取真实账号
cargo run -p codexbar-cli -- cards
cargo run -p codexbar-cli -- diagnose       # 打印解析到的路径与凭据状态
cargo run -p codexbar-app                   # 托盘 App（左键弹卡片，右键菜单）
```

无 Claude 凭据时验证 Claude 通路：

```powershell
python tools/mock-claude.py     # 另开一个终端
$env:CLAUDE_CONFIG_DIR="<含 .credentials.json 的目录>"
$env:CODEXBAR_CLAUDE_BASE_URL="http://127.0.0.1:8787"
cargo run -p codexbar-cli -- usage --provider claude
```

### 已验证（实机）

- Codex 真实账号：`usage` 输出 Session 76% left / resets in 5d 11h、Spark 双车道、Credits、Limit Reset Credits（1 available, next expires in 19d 9h）、Account、Plan Pro。
- Claude 通路（mock）：请求头 `Authorization: Bearer …`、`anthropic-beta: oauth-2025-04-20`、`User-Agent: claude-code/2.1.0 (external, cli)` 均正确；五条车道与倒计时映射正确。
- 托盘 App：启动后 4s 内完成首轮抓取，`tray icon updated` 落日志；自适应策略给出 30m（`longIdle`，菜单未打开）。
- 弹出层截图确认 Codex/Claude 两张卡片渲染正确（含 23% 车道的琥珀色告警）。
- 托盘图标实机截图确认：单车道 76% 填充；并据此修掉一个真实缺陷——图标原本恒用白色字形，在浅色任务栏几乎不可见，现按注册表 `SystemUsesLightTheme` 切换深/浅字形。

## 8. 已落地实现（M2：QuickJS 插件宿主 + 配置）

**上游 16 个 `.js` provider 现在原样运行**，一个字节都没改。文件 verbatim vendor 在
`crates/codexbar-core/resources/plugins/`（含上游 MIT LICENSE 与刷新命令）。

### 宿主契约实现（`crates/codexbar-core/src/plugin/`）

| 上游 | 本仓 |
|---|---|
| `provider-plugin-prelude.js`（JS 侧 ctx） | 原样 `include_str!` 后以 `(ctx, host)` 调用 |
| 原生 `host.*` 桥（`QuickJSProviderPluginEngine.swift:497-767`） | `engine.rs`：`http` / `settingGet` / `cookieHeader` / `cacheGet` / `cacheSet` / `log` / `nextDailyReset` / `pct` / `amountFromPercent` |
| `defineProvider` 全局 + manifest 校验（`ProviderPluginManifest.swift:81-252`） | `manifest.rs`：id 规则、name≤80、endpoints 1..16、settings≤32 去重、endpoint setting 必须 plain、auth secret 必须 secure、browser-cookies 必须带域名 |
| 网络策略（`:689-789`） | 仅 GET/POST；origin 必须匹配固定 origin 或按 policy(`https` / `https-or-loopback-http` / `https-or-private-network-http`) 匹配用户配置 origin；timeout 默认 15s、限 1..30；5 MiB 响应上限；注入 `Accept`/`Content-Type`/auth 头；禁止插件覆盖 auth 头 |
| 失败分类（`__CODEXBAR_FAILURE_V2__`） | `PluginFailure{kind,message,retry_after_seconds}`，retry 按上游 clamp 到 10s，并区分 transient / 需要凭据 |
| 快照映射（`ProviderPluginSnapshotMapper.swift:39-340`） | `snapshot.rs`：百分比 clamp、windowMinutes 正整数、Date/ISO 解析、extraWindows（inline 或 nested）、cost/costUsage 校验（货币 3 位、historyDays 1..366）、details 分节 + 图表、identity、dataConfidence、空快照拒绝 |
| `nextDailyReset(tz,hour)` | `timezone.rs`：内置 IANA tzdb（`time-tz`），QuickJS 无时区库，由宿主计算 |

关键实现约束：QuickJS 的 job 队列必须在 `Context::with` **之外**泵（`with` 持有 runtime 锁，
`execute_pending_job` 会二次借用直接 panic）。因此 `drive_fetch` 先在一次 `with` 内调用
`fetchUsage` 并把 Promise 挂到全局，再在锁外循环 `execute_pending_job` + 轮询 Promise 状态。
`host.http` 同步完成请求后立刻 resolve，整段执行跑在 `spawn_blocking` 线程上，通过
`tokio::runtime::Handle::block_on` 发起真实请求。

### 配置与 CLI

- `crates/codexbar-core/src/config.rs`：`%APPDATA%\CodexBar\config.json`，schema 与上游一致
  （`{version, providers:[{id, enabled, apiKey, pluginSettings, pluginSecrets, …}]}`），**未知字段原样回写**，
  保存后用 `icacls` 收紧 ACL。
- CLI 新增 `config providers|enable|disable|set-api-key|set|path`，`set-api-key --stdin` 避免密钥进 shell 历史。
- `providers::active_ids()`：只轮询「已启用 + 凭据齐备」的 provider；需要浏览器 cookie 的 4 个插件
  （manus / perplexity / qoder / t3chat）在 cookie 导入落地前一律标记 not ready，不产生周期性报错行。

### 已验证（实机）

- `cargo test`：110 个测试通过（含 `tests/plugin_host.rs` 三个端到端用例）。
- 端到端：本地 HTTP 服务 + 上游 `sub2api.js` 原文 → 校验插件自己拼的 `/v1/usage?days=30&timezone=UTC`、
  `Authorization: Bearer …`、`Accept: application/json`；快照回来 primary/secondary/tertiary + 两条 extraWindows +
  `Usage summary` 分节（`9,876,543` 千分位、`$250.50` 次要值）+ `subscriptionExpiresAt` + identity。
- 分类失败：mock 返 401 → `ctx.fail.authenticationExpired(...)` 穿过 QuickJS 边界成为 `authentication-expired`。
- CLI 实机：`config set-api-key --stdin` + `config set --key SUB2API_BASE_URL` 之后 `usage --provider sub2api`
  打印五条车道与 Usage summary；mock 端确认收到 Bearer 头。
- 托盘 UI 截图：插件 provider 与原生 provider 同屏渲染，含分节标题与 10%-left 车道的红色告警。

## 9. 已落地实现（M3：DPAPI 密钥存储 + 设置界面）

### 密钥存储（`crates/codexbar-core/src/secret.rs`）

macOS 版把 token 放 Keychain；Windows 对等物是 **DPAPI CurrentUser**（`CryptProtectData`）。

- 独立文件 `%APPDATA%\CodexBar\secrets.json`：`{"version":1,"entries":{"<provider>/<key>":"<base64 DPAPI blob>"}}`，
  与 config.json 分开，这样共享给 macOS 版的 config 里永远不会出现它解不开的密文。
- 每条 blob 绑定 entropy `codexbar:secret:v1:<provider>:<key>`：把密文挪到别的 key/provider 下解密即失败（有测试）。
- 写入后同样用 `icacls` 收紧 ACL；空值即删除条目。
- 威胁模型明说：DPAPI 防的是其他用户与离线拷贝，**不防同用户恶意进程**——后台 App 用 macOS Keychain 也一样。

### 解析优先级（`crates/codexbar-core/src/settings.rs`）

`Settings = Config + SecretStore`，是唯一知道凭据解析顺序的地方：
DPAPI 存储 → config 里残留的明文 `pluginSecrets`/`pluginSettings` → 通用 `apiKey`（仅当插件只声明一个 secret）→ 同名环境变量。
`migrate_plaintext_secrets()` 把旧的明文条目搬进加密存储并从 config 抹掉（幂等，有测试）。

### 设置窗口（`ui/settings.html|css|js` + `src-tauri` 命令）

- 左侧 provider 列表带状态徽标：`READY` / `SETUP`（缺凭据）/ `COOKIES`（等 cookie 导入）/ `OFF`；带过滤框。
- 右侧详情：启用开关、插件声明的每个设置项。**secure 字段只读“是否已存”**，值永不回传前端；
  提供 Save / Clear；plain 字段（如 `SUB2API_BASE_URL`）直接编辑。
- 原生 provider（Codex/Claude）显示说明：凭据来自各自 CLI，无需在此输入。
- 底部显示 config 与 secrets 的真实路径。托盘右键菜单与弹出层都能打开设置窗口。
- 命令：`provider_settings` / `set_provider_enabled` / `set_provider_secret` / `set_provider_setting` / `open_settings` / `hide_settings`。

CLI 同步升级：`config set-api-key|set-secret|clear-secret|migrate-secrets`，`config path` 同时打印两个路径。

### 踩到的坑（已修）

1. **在命令里按需创建 WebView 会死锁**：`WebviewWindowBuilder::build()` 需要主线程事件循环，而同步命令本身占着主线程 →
   窗口出现但网页永远不加载（表现为纯白窗口，`on_page_load` 不触发）。改为在 `tauri.conf.json` 声明
   `settings` 窗口且 `visible:false`，`show_settings` 只负责 show/focus。
2. **新增 UI 文件不会触发资源重嵌**：`tauri-build` 的 rerun-if-changed 不含 `ui/`，加了 `settings.html` 后必须
   `touch src-tauri/tauri.conf.json` 才会重新嵌入。

### 已验证（实机）

- `cargo test`：**123 个测试通过**（core 99 + 插件端到端 3 + app 15 + cli 6），零警告。
- DPAPI：`config set-api-key --stdin` 后 `config.json` 无密钥、`secrets.json` 存 `AQAAANCMnd8BFdER…` 密文；
  随后 `cards --provider sub2api` 成功取数，mock 端收到 `Authorization: Bearer sk-dpapi-test` —— 证明解密链路通。
- 设置窗口截图（PrintWindow 抓真实窗口）：`3 of 18 providers ready`、状态徽标、Codex 详情与说明、底部真实路径。
- 设置窗口 IPC：页面加载时 `provider_settings` 返回真实数据并渲染，证明 JS↔Rust 桥通；
  三个写入命令由 `src-tauri` 的 `settings_commands_read_and_write_real_files` 直接调用验证
  （写 secret → provider 变 ready 且 config 无明文；写 plain 设置 → 回读一致；禁用 → 退出活跃集；清空 → 恢复未配置）。

**未验证的部分（诚实说明）**：设置窗口里 Save/Clear/开关的**鼠标点击**未能自动化验证——本机 Orca 持有前台锁，
`SetForegroundWindow`/`SwitchToThisWindow` 都无法把设置窗口切到前台，非活动 WebView2 会丢弃合成点击。
已验证的是同一批命令函数与 IPC 桥本身。

## 10. 已落地实现（M4：本地成本库 + 状态轮询）

### 定价（`crates/codexbar-core/src/cost/pricing.rs`）

照抄 `CostUsagePricing.swift:68-207`（Codex 23 个模型）与 `:246-420`（Claude 8 个模型）的**每 token** 费率，
含长上下文分档（>272K / >200K 输入的整请求换档）、cache read/write 单独费率、`gpt-5.3-codex-spark` 免费且标 "Research Preview"。
语义与上游一致：`cache_read` 缺失 ⇒ 缓存输入按普通输入计价；`cache_write` 缺失 ⇒ 按未缓存输入计价；
**未知模型返回 `None` 而不是瞎猜**（照抄上游 `:575-605`），CLI/UI 用 "some models unpriced" 明示这是下界。
模型名归一化：剥掉路由前缀（`openai/`）与日期后缀（`-20250929`），变体名走最长前缀匹配。

### 扫描（`cost/scanner.rs`）

- **Codex**：`CODEX_HOME/sessions` + 同级 `archived_sessions` 递归找 `.jsonl`；
  取 `type=event_msg` + `payload.type=token_count` + `payload.info`，模型优先级 `info.model` → `turn_context.payload.model` → `thread_settings.model`。
- **关键语义（实测确认）**：`info.total_token_usage` 是**会话累计值**。用真实 6 MB 会话验证过：
  直接累加 `last_token_usage` 得 9.11M 输入，而末尾累计值只有 6.43M —— 事件会重复。
  因此按上游 `:700-815` 的 watermark 做差；累计值**变小**说明发生了 compaction/重启，该次全量计入（那个会话里出现 2 次）。
- **Claude**：`CLAUDE_CONFIG_DIR/projects/**/*.jsonl`，取 `type=assistant` + `message.usage`，
  字段 `input_tokens` / `cache_creation_input_tokens`（含 `cache_creation.ephemeral_1h_input_tokens`）/ `cache_read_input_tokens` / `output_tokens`；
  流式分片按 `message.id:requestId` 去重，**最后一片胜出**。
- 断点续扫：记录 `parsed_bytes` + watermark + 已计 message key；文件变短（轮转）则整份重读。

### SQLite 存储（`cost/store.rs`）

`%LOCALAPPDATA%\CodexBar\cache\cost-usage\cost-usage.sqlite`，WAL + `foreign_keys` + 5s busy timeout，
预算与上游一致（25,000 行 / 256 MiB，`CostUsageStore+CodexCache.swift:46-47`），按天窗口保留 400 天。

**明确说明这是上游 13 张表的子集**：保留 `files`（续扫状态）、`file_day_aggregates`（按文件可重算）、
`day_aggregates`（UI 读取）、`meta`。上游另有 `token_snapshots`/`usage_rows`/`buffered_lines`/`fork_lineage`/
`discovery_state`/`lookback_state`/`accumulators`，用于 fork 归属与半行缓冲——本移植未做，代码注释里写明是**有意省略**。

### 状态轮询（`crates/codexbar-core/src/status.rs`）

两个请求（照抄 `UsageStore+Status.swift`）：`/api/v2/status.json` 取总指示灯 + `page.updated_at`，
`/api/v2/components.json` 取组件行（按 `position` 排序、丢空名）。超时 10s。
指示灯映射照抄 `UsageStoreSupport.swift:66-79`；托盘叠加照抄 `IconRenderer.swift:1009-1045`：
minor/maintenance 画点，major/critical/unknown 画感叹号。
`OutageTracker` 复刻 `HookTransitionDetector.swift:161-220` 的边沿触发：首次观测只建基线，
maintenance/unknown 不动基线，轮询失败保留上次状态（不会把网络抖动当宕机）。

### 修掉的两个真实缺陷

1. **成本翻倍**：同一文件因路径拼写不同（`CODEX_HOME` 用 `/` 还是 `\`）在 `files` 表里存了两行，
   `day_aggregates` 于是把同一份数据加了两遍（实测 264 → 528 请求、$22.59 → $45.18）。
   现在入库前 `canonicalize` + 统一分隔符与大小写；新增测试 `the_same_file_spelled_differently_is_one_row`。
2. **隐藏窗口抢焦点**：`settings` 窗口虽然 `visible:false`，其 webview 初始化时仍会取得焦点，
   触发弹出层的 blur-hide，导致弹出层刚显示就消失。已给该窗口加 `"focus": false`。

### CLI

- `codexbar cost [--days N] [--group-by day|model|provider] [--json]`：先增量扫描再汇总，
  输出扫描统计（scanned/new events/unchanged）+ 窗口总额 + 分组表。
- `codexbar status [--provider id] [--json]`：逐个 provider 打指示灯与非正常组件行。
- `diagnose` 增加 `cost.db` 路径。

### 已验证（实机）

- `cargo test`：**161 个测试通过**，零警告。
- **真实 Codex 会话日志**（4 个文件，6.4 MB 最大）：`cost --days 7` → 264 事件、27,355,657 tokens、**$22.59**；
  `--group-by model` 拆出 `gpt-5.6-sol $22.18` / `gpt-5.6-terra $0.41`。
- **增量续扫**：第二次运行 `scanned 0 of 4 (4 unchanged)`，总额分毫不变；换路径分隔符再跑仍不变（翻倍缺陷已修）。
- **真实状态页**：`codexbar status` 对 status.openai.com / status.anthropic.com 均返回 `none · All Systems Operational`。
- **托盘弹出层截图**：Codex 卡片出现 `Today $0.51 (local scan)` 与 `Last 30d $22.59 (local scan)`，与 CLI 一致。

**未验证的部分（诚实说明）**：事故横幅与感叹号叠加只有单测覆盖（指示灯映射、叠加选择、图标像素、`has_incident` 接线），
因为两家状态页当前都正常，无法制造真实事故；provider 的状态 URL 是描述符里硬编码的，没有为测试留 override。

## 11. 已落地实现（M5：cookie 导入 + 手工 cookie）

### 本机实测结论（这决定了整个设计）

| 浏览器 | 状态 |
|---|---|
| Chrome（运行中） | `Network\Cookies` 被独占锁；即便用 `CreateFileW` 带全部 share flag 仍返回 `ERROR_SHARING_VIOLATION`(32) |
| Edge（未运行） | 可读，但 **200 条 cookie 全部是 `v20`**（App-Bound Encryption），用户态进程无法解密 |
| Firefox | 未安装 |

所以 §5 里预判的"Chrome v20 是命门"在本机被完整证实。设计因此把**手工粘贴 header 放在第一位**，
浏览器导入作为可选增强，并且每种失败都给可执行的下一步。

### 实现（`crates/codexbar-core/src/cookies/`）

- `chromium.rs`：浏览器发现（Chrome/Edge/Brave/Vivaldi/Chromium）、profile 枚举（`Default` 优先）、
  `Local State` → `os_crypt.encrypted_key` 去 `DPAPI` 前缀后**无 entropy** DPAPI 解包 → AES-256-GCM 解 `v10`/`v11`
  （AAD 先试 `host\0name` 再试空，覆盖新旧 Chromium 绑定）；`v20` 直接报 `AppBoundEncryption`；
  无版本前缀的老 profile 走裸 DPAPI；文件读取用 `CreateFileW` + 全 share flag，锁定时报 `BrowserLocked` 并提示关闭浏览器。
- `firefox.rs`：`%APPDATA%\Mozilla\Firefox\Profiles\*\cookies.sqlite` 的 `moz_cookies`，**值是明文**，
  `default-release` profile 优先。这是 Chromium 全面 v20 后 Windows 上唯一还能自动导入的路径。
- `mod.rs`：域匹配（`t3.chat` 同时匹配 `t3.chat`/`.t3.chat`/`api.t3.chat`，但拒绝 `nott3.chat`）、
  header 序列化（同名去重、丢空值）、导入顺序（Firefox → Chromium）、`ImportReport` 逐浏览器解释为什么没导到。

### 凭据与解析优先级

手工 header 存进 DPAPI 存储（保留键 `__cookie_header`），**不写进 config.json**（上游把 `cookieHeader` 放配置文件，这里有意不这么做）。
`cookieSource` 沿用上游语义 `off | manual | auto`（默认 auto），解析顺序：手工 → 浏览器导入。
`can_supply_cookies()` 做便宜的就绪判定：有手工 header 直接算就绪；`auto` 只在存在 Firefox profile
或**没有** `app_bound_encrypted_key` 的 Chromium profile 时才算就绪——所以本机上那 4 个 cookie 插件
不会每轮刷新都刷一条注定失败的错误行（fetch 时仍会真正尝试导入，因为 v20 profile 里可能残留老 `v10` 值）。

### 插件桥接

`host.cookieHeader(domain)` 从"平台不支持"的桩换成真实解析器，并保留上游的两道闸门：
manifest 必须声明 `browser-cookies` capability，且请求的域必须在 `cookieDomains` 里。
解析器由 `Settings::cookie_resolver_for()` 注入，引擎本身不认识配置与密钥存储。

### CLI 与设置界面

- `codexbar cookie [--domain d] [--json]`：列出 Firefox/Chromium profile、逐库导入结果与失败原因、
  以及每个 cookie 插件的 source/manual/声明域。**只打印 cookie 名字与数量，绝不打印值**。
- `codexbar config set-cookie --provider <id> --stdin` / `config cookie-source --provider <id> --value off|manual|auto`。
- 设置界面：cookie 类 provider 多出 "Cookie source" 下拉与 "Cookie header" 密码框（stored 徽标 + Save/Clear），
  提示里写明声明域、DPAPI 加密、以及 Chrome 127+ 需要从 DevTools 复制 header。
- 顺带做了 provider 深链：弹出层的 Settings 按钮会带上当前 provider，设置窗口按 `#<provider>` 直接定位。

### 已验证（实机）

- `cargo test`：**188 个测试通过**，零警告。
- **真实 AES-GCM + 真实 DPAPI**：用本机 DPAPI 包一个 Chromium 式 master key 再解包，并用真算法生成 `v10` 值解密成功；
  换错 key 或换错 host（AAD）都必须失败而不是返回垃圾。
- **真实 Edge profile**：`codexbar cookie --domain bing.com` → `app-bound encryption (v20) … paste the cookie header manually or use Firefox`。
- **真实 Chrome（运行中）**：同一命令报 `is locked by the running browser; close it or paste the cookie header manually`。
- **手工 header 全链路**：`config set-cookie --stdin` → `secrets.json` 只有密文（校验过无明文）→ `config providers` 里 t3chat 变 READY；
  集成测试 `a_pasted_cookie_header_reaches_the_plugins_request` 用一个测试专用插件证明 header 真的出现在 HTTP 请求头里。
- **闸门**：未声明的域被拒且**不发请求**；`cookieSource=off` 时连解析器都不构造。
- **设置界面截图**：T3 Chat 详情页出现 Cookie source 下拉与带 stored 徽标的 Cookie header 输入框。

**未验证的部分（诚实说明）**：Firefox 导入路径只有合成 `cookies.sqlite`（真实 schema）覆盖——本机没装 Firefox；
Chromium `v10` 解密走的是我用真算法自造的值，不是真浏览器写的 `v10`（本机两个浏览器都已全量 v20，无法取样）。

## 12. 生产缺陷修复：托盘 App 会自己消失

**现场**：一个跑了约 3.5 小时的实例在 19:46 自行退出（exit code 0），日志末尾只有
`[ERROR:ui\gfx\win\window_impl.cc:124] Failed to unregister class Chrome_WidgetWin_0. Error = 1412`
（`ERROR_CLASS_HAS_WINDOWS`，来自 WebView2 拆卸）。托盘工具无声消失是不可接受的。

### 根因链（三个独立缺陷，逐个实测确认）

1. **Tauri 在最后一个窗口消失时请求退出**，而本 App 的窗口平时都是隐藏的。
   WebView2 运行时被替换/崩溃 → 窗口被销毁 → 退出请求 → 整个 App 跟着走。
   修法：`.build(ctx).run(|_, event| …)`，只有携带 code 的退出（即 `AppHandle::exit`，也就是显式 Quit）才放行，
   其余一律 `api.prevent_exit()`。判定抽成纯函数 `should_prevent_exit(code)` 并单测两个分支。
2. **webview 死后窗口壳还在，内容全白**（实测：杀掉 7 个 `msedgewebview2.exe` 后窗口句柄仍有效，
   `PrintWindow` 截出来是纯白）。所以"不退出"只是把崩溃换成了僵尸 UI。
   修法：60s 看门狗 + `ping`/`ui_ready` 心跳（前端两个页面都应答），700ms 无应答即判定死亡。
3. **`WebviewWindow::reload()` 救不回来**：宿主进程已死，reload 返回
   `HRESULT(0x8007139F) ERROR_INVALID_STATE`（实测日志）。必须重建窗口。
   而 `destroy()` 是**异步**的——先 destroy 再立刻重建会撞
   `a webview with label 'main' already exists`（实测日志）。
   修法：看门狗只负责 destroy，重建放到 `WindowEvent::Destroyed` 里做（此时 label 已释放），
   并用 `quitting` 标志区分"用户退出"与"崩溃"，避免退出时无限重建。

顺带把两个窗口的定义从 `tauri.conf.json` 移进代码（`build_popover` / `build_settings`），
让启动与崩溃后重建共用同一份参数——否则重建会悄悄丢掉 `always_on_top` / `focus:false` 这类属性。

### 已验证（实机，完整恢复链）

杀掉该实例全部 7 个 `msedgewebview2.exe` 后，日志按顺序出现：

```
WARN window did not answer; tearing down its dead webview label="settings"
WARN window did not answer; tearing down its dead webview label="main"
WARN window destroyed; rebuilding label=settings
WARN window destroyed; rebuilding label=main
WARN ignoring exit request with no code; staying in the tray   ← 原缺陷正是在这里退出的
INFO rebuilt window after webview loss label="main"
INFO rebuilt window after webview loss label="settings"
```

随后 `msedgewebview2.exe` 恢复为 7 个，截图确认重建后的设置窗口渲染完整（provider 列表、状态徽标、真实路径），
托盘图标在整个过程中持续正常刷新。`cargo test`：**190 个测试通过**，零警告。

**遗留**：无法在本机复现最初那次退出的**触发源**（WebView2 自动更新最可能，但不可按需触发）。
现在无论触发源是什么，退出请求都会被拒绝并留下日志，所以下次若仍发生，日志足以定位。


## 13. 已落地实现（M6：受限 Windows 子进程执行器）

### 实现（`crates/codexbar-core/src/subprocess.rs`）

- 直接调用 `CreateProcessW`，不经过 shell；参数按 `CommandLineToArgvW` 规则编码，覆盖空参数、
  空格、引号和引号前反斜杠。
- 调用方必须传入**完整环境变量表**，不会默认把 CodexBar 进程里的 token、cookie、CI/cloud
  secret 泄漏给 CLI。按传入环境的 `PATH` 解析裸命令名，不偷偷回退到宿主环境。
- stdout/stderr 使用两条匿名管道并发持续排空，两个流分别限额（默认 1 MiB）；
  超限后继续排空避免子进程阻塞，但最终 fail-closed 返回 `OutputTooLarge`。
- 用 `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` 把继承面限制为 stdin/stdout/stderr 三个句柄，
  避免并发刷新时两个 provider 互相继承管道导致永不 EOF。
- 用 `PROC_THREAD_ATTRIBUTE_JOB_LIST` 在进程创建时**原子**加入 Job Object，没有
  “先启动、后入组”期间逃逸子进程的竞态。Job 带 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`：
  超时会终止整棵树，根进程自然退出后残留的后代也会被清理。
- 错误分类覆盖 binary not found、非法输入、launch/job 失败、超时、输出超限和非零退出；
  非零退出保留 stderr，调用方可按 provider 语义映射。
- Windows-only 实现在非 Windows 构建上返回明确的 `UnsupportedPlatform`，不是静默 no-op。

### 已验证（实机）

- 公共 API 直接启动真实 `%SystemRoot%\System32\cmd.exe`，显式环境变量成功到达子进程，
  stdout/stderr 分流和退出码均正确。
- 测试进程递归启动孙进程：300ms 超时后孙进程未能写出延迟 marker，证明 Job 终止整棵树。
- 根进程自然退出、孙进程仍存活且可能继承管道的路径也被覆盖：runner 能立即收尾，孙进程未逃逸。
- 另有非零退出 + stderr、双流并发捕获、输出限额、环境块排序/双 NUL 结尾和 Windows 参数引用测试。
- `cargo test --workspace`：**198 个测试通过**，1 个仅供子进程自调用的 helper test 按设计 ignored，零失败。

### 当前边界

- `amp` / `augment` / AWS profile 三条真实 CLI 调用链已在 M9 接入；Gemini 按上游契约读取
  CLI OAuth 状态后直连 Cloud Code API，不伪造不存在的 `gemini usage` 命令。
- 不自动执行 `.cmd` / `.bat`；需要脚本包装器的 provider 必须显式、固定地调用 `cmd.exe`，
  不能把用户输入拼进命令字符串。
- 不提供终端仿真；kiro/grok/antigravity 仍需要后续 ConPTY。

## 14. 已落地实现（M7：十个 API 插件转换）

### 覆盖

- 新增 `aiand`、`chutes`、`deepinfra`、`elevenlabs`、`fireworks`、`litellm`、
  `llmproxy`、`moonshot`、`neuralwatt`、`zenmux` 十个 Windows-first QuickJS 插件。
- provider registry 从 **18/69** 扩到 **28/69**：2 个原生 provider + 26 个插件；
  `codexbar providers --json` 实机返回 28 条。
- 请求端点、认证头、区域路由、分页/配额 fallback 和 payload 字段逐项对齐上游 Swift
  provider；转换版与 16 个原样 vendor 的上游插件分开标记，避免把手工转换误称为 verbatim。
- 所有插件继续经过宿主网络策略：固定 origin 默认只允许 HTTPS；LiteLLM 等用户配置 endpoint
  仅允许 HTTPS、loopback HTTP 或插件声明的 private-network HTTP，插件不能覆盖认证头。

### 已验证（实机）

- `crates/codexbar-core/tests/plugin_expansion.rs` 用上游响应形状的本地 fixture 驱动真实
  QuickJS 引擎、HTTP broker 与 snapshot mapper；十个插件全部完成请求并生成预期窗口、
  credits、cost、identity 或 details。
- bundled registry 测试锁定 26 个插件的精确 id 与源码，逐个解析并校验 manifest。
- `cargo test --workspace`：**199 个测试通过**，1 个子进程 helper test 按设计 ignored，零失败。
- 当前环境没有这十家服务的真实凭据，因此没有声称线上账号验证；本地测试覆盖的是实际
  插件执行和 HTTP 路径，不是仅调用 mapper 的单元测试。


## 15. 已落地实现（M8：Windows 产品化）

### 桌面集成

- `tauri-plugin-notification` 接入原生 Windows Toast。每个 provider 的最紧张窗口首次达到
  90% used 时告警；持续高位不重复发送，回落到 85% 以下才重新布防，避免刷新和数据抖动刷屏。
- `tauri-plugin-global-shortcut` 注册 `Ctrl+Shift+Space`，从任意应用切换托盘弹窗；
  注册冲突只记 warning，不阻止托盘启动。
- `tauri-plugin-autostart` 提供当前用户级登录启动，设置窗口可读写真实注册状态；
  自启参数固定为 `--autostart`，不经过 shell。

### Velopack 发行与回滚

- `VelopackApp::build().run()` 是进程第一段初始化，安装/更新 hook 不会先启动 Tauri。
- `src-tauri/src/updater.rs` 从编译期 `CODEXBAR_UPDATE_URL` 固定 HTTPS feed，检查、下载后使用
  `wait_exit_then_apply_updates` 等待应用正常退出再替换文件；启用 `AllowVersionDowngrade`，
  feed 撤回坏版本时可下载完整旧版并清除更新版缓存。
- `tools/package-windows.ps1` 从 workspace manifest 读取版本，构建 release、隔离 staging，
  再用 `vpk pack` 生成 current-user 安装包、portable 包、full/delta 更新包；AUMID 固定为
  `app.codexbar.windows`，同时给 Toast 和开始菜单快捷方式提供稳定身份。
- Tauri NSIS bundler 已关闭，避免同一版本产生两套安装/更新所有权。当前工作站缺少 `vpk`
  所需的 .NET SDK 8，脚本已实测 fail-fast；安装包产物本机尚不可生成。

### 本地化

- `ui/i18n.js` 提供 English、简体中文和繁体中文三套 UI 文案；首次按系统语言选择，
  `zh-TW` / `zh-HK` / `zh-MO` 归一到 `zh-Hant`，其他中文归一到 `zh-Hans`，未知语言回退英文。
- 语言选择保存在同源 `localStorage`，设置窗与弹窗通过 storage/custom event 同步，不需重启。
- Chromium 实际渲染验证简体中文标题、按钮、筛选框、登录启动、更新和语言选择器均已切换；
  独立状态机验证 `zh-CN`、`zh-TW` 与非支持语言回退。

### 已验证

- `cargo test --workspace`：**202 个测试通过**，1 个子进程 helper test 按设计 ignored，零失败。
- `node --check` 覆盖全部三个 UI 脚本；Chromium 无障碍树确认简体中文实际渲染。
- 托盘进程在接入 Velopack、通知、自启和全局快捷键插件后完成真实 Codex 刷新并更新图标；
  启动日志没有快捷键注册冲突或插件初始化错误。
- 安装/升级/回滚产物仍受本机缺少 .NET SDK 8 / `vpk` 阻塞，未把“配置可编译”误报为
  “安装包已实机升级”。**（该阻塞已在 §17 解除并完成实机验证。）**

## 17. 发行链路实机打通（安装 / 升级 / 回滚）

装上 .NET SDK 8.0.424 + `vpk` 1.2.0（与 `velopack` crate 同为 1.2.0）后，`tools/package-windows.ps1`
首次真实产出安装包：`Setup.exe`(12.5 MB)、`Portable.zip`、`*-full.nupkg`、后续版本的 `*-delta.nupkg`
以及 `releases.win.json`。

### 为了验证而做的两处改动

- **`--check-update` / `--apply-update` 无头入口**（`src-tauri/src/main.rs`）。更新逻辑原本只挂在设置窗口上，
  而设置窗口无法自动化点击，所以「安装包真的能升级」这件事没有任何可脚本化的证明路径。
  两个开关调用的是设置窗口调的同一组 `updater` 函数，打印 JSON 后退出；`--autostart` 等参数不受影响（有单测）。
- **打包脚本允许 loopback HTTP feed**。真实 feed 地址尚未确定（仓库还没有远端），因此用本地
  `http://127.0.0.1:8799/` 起了一个静态 feed 完成全流程。feed 地址是编译期常量，测试构建不可能被误当成发布构建。

### 顺带修掉的一个真实缺陷

WebView2 的用户数据目录默认是 `%LOCALAPPDATA%\<bundle identifier>\EBWebView`，而这**正是 Velopack 的安装根目录**
`%LOCALAPPDATA%\app.codexbar.windows\`。任何先跑过 App、再装安装包的机器上，安装器都会因为清不掉这个残留 profile
而失败（实测报 `Failed to remove existing application directory`）。现在两个窗口都显式指向
`%LOCALAPPDATA%\CodexBar\webview`（`webview_data_directory()`）。

### 已验证（实机，真实安装目录）

必须在 Claude 桌面版的 MSIX 容器**之外**跑：容器内 `%LOCALAPPDATA%` 被重定向到
`C:\WpSystem\…\Packages\Claude_…\LocalCache\Local\`，Velopack 的文件搬运会跨重定向边界报
`os error 17 (CrossesDevices)`。这是容器假象，不是 App 缺陷——改用计划任务在容器外执行后全部通过：

| 阶段 | 结果 |
|---|---|
| 全新安装 | `Setup.exe --silent` exit 0，`current\sq.version` = 0.1.1，`--check-update` → `upToDate` |
| 回滚（feed 撤回 0.1.1） | `--check-update` → `rollbackAvailable` 0.1.0 → `--apply-update` → 实际降级到 0.1.0，App 被更新器重启 |
| 升级（feed 恢复 0.1.1） | `--check-update` → `updateAvailable` 0.1.1 → `--apply-update` → `{"scheduled":true}` → 0.1.1 → `upToDate` |
| 卸载 | `Update.exe --uninstall --silent` exit 0，安装目录已删除 |

`cargo test --workspace`：**215 个测试通过**，1 个子进程 helper test 按设计 ignored，零警告。

### 仍未解决

- **代码签名**：`vpk` 每次都警告 `No signing parameters provided`，未签名 exe 首次运行会被 SmartScreen 拦。
- **卸载残留**：Velopack 只删自己的安装目录，`%LOCALAPPDATA%\CodexBar\webview` 会留下。配置与密钥
  （`%APPDATA%\CodexBar\`）留下是对的，webview profile 属于缓存，后续可挂 Velopack 卸载钩子清理。

## 16. 已落地实现（M9：首批 CLI 与 CLI-owned 提供商）

### Amp 与 Augment

- `crates/codexbar-core/src/providers/amp.rs` 固定执行 `amp usage`，注入
  `NO_COLOR=1` / `TERM=dumb`，优先 stdout、
  空 stdout 时读取 stderr；解析金额/百分比 Free tier、subscription 的 other/orb 双窗口、
  登录身份及个人/工作区余额。
- `crates/codexbar-core/src/providers/augment.rs` 固定执行 `auggie account status`，同时兼容旧版
  `remaining · used / total` 和新版 `credits remaining` 输出，并解析 billing-cycle 结束日期。
- 两者只接受原生 `.exe` / `.com`；不会隐式执行 `.cmd` / `.bat`，参数始终以数组传给
  `CreateProcessW`，没有 shell 拼接面。

### AWS Bedrock

- `crates/codexbar-core/src/providers/bedrock.rs` 优先读取 DPAPI/环境中的 access key；配置
  AWS profile 时执行
  `aws configure export-credentials --profile <name> --format process`，因此兼容 SSO、
  assume-role 和 `credential_process`。
- CLI 返回的临时凭据只驻留内存。Cost Explorer 请求在本地完成 SigV4 签名，支持 session
  token、分页、重复 page token 防护及 `DataUnavailableException` 零用量语义。
- 月度查询上界使用“明天”而非未来月末，结果只累计 service key 含 Bedrock 的分组；
  可配置月预算，统一模型同时暴露预算窗口和 USD cost snapshot。

### Gemini

- 上游 Gemini 并不是 quota CLI 命令：`gemini` 负责 `%USERPROFILE%\.gemini` OAuth 状态，
  CodexBar 调用 Cloud Code API。因此 Windows 端读取同构 `oauth_creds.json`，拒绝 API-key /
  Vertex AI 模式，必要时用 Gemini CLI bundle 或显式环境变量中的 OAuth client 刷新 token。
- 先调用 `loadCodeAssist` 取得 managed project/tier，再调用 `retrieveUserQuota`；按 Pro、
  Flash、Flash Lite 分组，每组保留最紧张 bucket，映射成三个 24 小时窗口。

### 注册与验证

- 四家均为 native descriptor；未安装 CLI、缺 AWS 凭据或缺 Gemini OAuth 文件时不会进入
  active refresh，避免首次启动产生无意义错误。AWS profile/access key/secret/session token/
  monthly budget 可直接从设置窗口配置，secret 继续由 DPAPI 保存。
- fixture 编译成真实 Windows PE 子进程，端到端覆盖 `amp usage`、
  `auggie account status` 和 AWS `export-credentials` 的参数、显式环境、stdout 与解析链。
- parser fixture 覆盖 Amp subscription/free/balance、Augment 新旧格式、Bedrock 分组成本和
  SigV4 session token、Gemini OAuth bundle 与多模型最紧张 quota。
- `cargo test --workspace`：**214 个测试通过**，1 个子进程 helper test 按设计 ignored，
  零失败。本机没有安装 `amp` / `auggie` / `aws` / `gemini`，因此没有声称真实账号线上验证。

### 下一步（M10）

为确实需要交互式终端的 kiro/grok/antigravity 接入 ConPTY；保持 PTY provider 与本阶段
非交互、无 shell 的 CLI adapter 分层。

---

## 附：并行侦查产出（完整原文）

`agent://ProviderArch`、`agent://PlatformDeps`、`agent://UILayer`、`agent://CLIAndData`、`agent://WinFeasibility`、`agent://SwiftOnWindows`

## 18. 发行渠道落到 GitHub Releases

仓库：<https://github.com/williamwue/codexbar-plus>（public，MIT）。feed 就是仓库本身。

### 为什么要分两种 source

GitHub Releases 不是静态文件 feed —— 产物挂在 releases API 下面，必须用 `GithubSource`。
`updater.rs` 因此按 feed URL 的 **host** 选择 source：`github.com` 走 `GithubSource`，其余走 `HttpSource`。
静态 feed 保留不是历史包袱：它是唯一能在**不发布任何东西**的前提下，在本地把
安装/升级/回滚跑一遍的方式（§17 就是这么验的）。host 判定有单测，
`https://releases.example.com/github.com/x` 和 `https://notgithub.com/x/y` 都必须判成静态。

**不编译 access token**：仓库是 public，而把 token 编进分发出去的 exe，等于把作者的 GitHub
凭据交给每个拿到安装包的人。匿名调用限速 60 次/小时/IP，对"一个桌面应用查更新"绰绰有余。

`package-windows.ps1` 新增 `-Publish`（上传到 GitHub Releases）与 `-NoDraft`（直接发布，
否则留 draft）。token 从 `GITHUB_TOKEN` 或 `gh auth token` 取，不进命令行、不进 shell 历史。

### 已验证（实机，真实 GitHub feed，容器外）

| 步骤 | 结果 |
|---|---|
| draft 上传 | 5 个产物（Setup.exe / Portable.zip / nupkg / releases.win.json / RELEASES）就位 |
| draft 对客户端不可见 | 已打包的 App `--check-update` → `emptyFeed`（证明真的打到了 releases API） |
| 发布 v0.1.0 | 同一个 App 立刻变 `upToDate` |
| 从 GitHub 安装 | 下载已发布的 `Setup.exe --silent` → 装出 0.1.0，对真实 feed 报 `upToDate` |
| 升级 | 发布 v0.1.1 → `updateAvailable 0.1.1` → apply → 实际升到 0.1.1 → `upToDate` |
| 撤回回滚 | 删除 v0.1.1 release → apply → 实际降回 0.1.0 |

验证完把 v0.1.1 release 与 tag 删掉了，仓库只留 v0.1.0。

**一个诚实的观察**：删掉 v0.1.1 之后紧接着的那次 `--check-update` 仍报 `upToDate`，6 秒后的
`--apply-update` 才拿到降级目标并执行。这不是代码问题，是 GitHub releases API 在撤回后的
短暂缓存/最终一致性 —— 拿 GitHub Releases 当 feed 就会有这个窗口期，撤回坏版本后客户端
不是瞬间全部回滚。
