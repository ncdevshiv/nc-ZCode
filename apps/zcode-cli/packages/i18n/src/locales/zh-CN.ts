import type { ZCodeCopy } from "../types.js";

export const zhCN: ZCodeCopy = {
  locale: "zh-CN",
  cli: {
    errors: {
      localeUnsupported: (value) =>
        `Unsupported --locale value: ${value}. Supported languages: en-US, zh-CN, auto.`,
    },
    help: (version) => `zcode ${version}

用法:
  zcode [command] [options]

不传 command 时，zcode 会打开全屏 TUI。

命令:
  app-server 运行 ZCode Protocol stdio app server
  commands   列出自定义 slash commands（\`commands list\`）
  doctor     检查运行时和打包假设
  login [zai|bigmodel]  通过浏览器授权登录
  logout     删除共享的 Z.AI 登录凭据
  plugins    管理插件与市场（\`plugins list|install|uninstall|enable|disable|update|validate|marketplace ...\`；别名 plugin）
  skills     列出本地 skills（\`skills list\`）
  tui        打开终端 UI
  version    打印 CLI 版本

选项:
  -h, --help       显示帮助
  -v, --version    显示版本
  -p, --prompt <text>  单次运行 prompt，不打开 TUI
  --memory-bench   配合 --prompt 开启自动 Memory 提取并等待后退出（需已开启 Memory）
  --browser-use <mode> 启用 Browser Use backend（当前支持：headless）
  --surface <surface>  设置无头 prompt/app-server 的呈现面：terminal 或 desktop
  --browser-executable <path> headless Browser Use 使用的 Chrome/Chromium 路径
  --attach <path>  给 --prompt 附加本地文件；可重复传入
  --cwd <path>     从指定目录运行命令
  --disallowed-tools, --disallowedTools <tools...>
    从本次 prompt/TUI 的可用工具集中移除整个工具，不修改持久化配置。
    工具名用逗号或空格分隔，例如 "Bash Edit"。
    "Bash(git *)" 也会移除整个 Bash，不支持按命令内容匹配。
  --force-mcs      对 Anthropic provider 强制启用 mid-conversation system 投影
  --locale <locale>  UI 语言：en-US、zh-CN 或 auto
  --mode <mode>    prompt 权限模式：build、edit、plan 或 yolo（--prompt 默认 yolo）
  --resume <sessionId>  按 sessionId 恢复持久化 session（sess_...）
  --target <text>  在 headless 模式运行或设置 session goal
  --target-replace 替换 --target 已存在的 goal
  -c, --continue        恢复当前目录最近的 session
  --json           在支持的命令中输出机器可读 JSON
  --no-browser     不打开浏览器，只打印 OAuth URL
  --no-color       禁用 ANSI 颜色
  --verbose        打印更多诊断信息

Slash Commands:
  /help [command]       显示 slash command 帮助
  /login                使用 Z.AI OAuth 登录
  /logout               删除共享的 Z.AI 登录凭据
  /compact [instructions]  压缩当前对话
  /expert [status|resume|stop|<task>]  运行或管理 expert workflow
  /dwf [list|cancel|resume]  列出、取消或恢复 dynamic workflow run
  /fork [latest|checkpointId]  从 workspace checkpoint 派生新 session
  /mcp [list|status|connect|disconnect]  查看或管理 MCP servers
  /mode [mode]          查看或切换权限模式：build、edit、plan 或 yolo
  /model [id]           查看或切换当前 session 模型
  /new                  在 TUI 中开始新 session
  /resume [sessionId]   按 sessionId 恢复 session；省略时恢复当前 cwd 最新 session
  /rewind [latest|checkpointId]  查看最新 checkpoint 或恢复 workspace 文件
  /skill [name] [task]  列出 skills，或强制下一次 prompt 加载某个 skill
  /goal [action]        查看或设置当前 session goal
`,
  },
  tui: {
    copy: {
      copied: "The selected text has been copied to the clipboard.",
      failed: "Unable to copy selected text.",
      unavailable: "Text clipboard copying is not available in the current terminal.",
    },
    effort: {
      disabled: "closure",
      enabled: "turn on",
    },
    input: {
      activeStatusHint: "esc to interrupt",
      busyPlaceholder: "Input content will be queued",
      placeholder: "Enter prompt word",
      queuedMore: (count) => `There are ${count} items queued`,
      queuedSubmitHint: "Submitted after the next tool call.",
      queuedTitle: (count) => `Queue(${count})`,
      title: "enter",
      noHistorySource: "The input history source is not configured.",
      noPreviousInput: "There are no earlier inputs for the current project.",
      restoredPreviousInput: "The previous entry has been restored.",
      restoredPreviousInputWithAttachments: (count) => `The previous input has been restored, containing ${count} attachments.`,
      restorePreviousInputFailed: "Unable to restore last entry.",
      typePrompt: "Press Enter after typing your question.",
    },
    loginRequired: {
      help: "Enter /model to view the model, or enter /login to connect to the Coding Plan account.",
      message: "No models available, please configure Provider or enter /login to log in.",
      status: "No models available, please configure Provider or enter /login to log in.",
      title: "Need to configure model",
    },
    loginSetup: {
      emptyMessage: "No login options available.",
      help: "Use Up/Down to select and Enter to confirm.",
      options: {
        bigmodelApiKey: {
          inputPrimary: "Enter BigModel Coding Plan API Key",
          inputSecondary: "Paste the key here and it will be hidden while typing.",
          primary: "BigModel Coding Plan API Key",
          secondary: "Manually paste the Coding Plan API key.",
        },
        bigmodelOauth: {
          pendingPrimary: "Waiting for BigModel authorization",
          pendingSecondary: "Please complete the login in the browser, and the configuration will automatically continue after the authorization is successful.",
          primary: "BigModel Coding Plan",
          secondary: "Open the browser and log in, and the CLI will automatically query the authorization results.",
        },
        zaiApiKey: {
          inputPrimary: "Enter Z.AI Coding Plan API Key",
          inputSecondary: "Paste the key here and it will be hidden while typing.",
          primary: "Z.AI Coding Plan API Key",
          secondary: "Manually paste the Coding Plan API key.",
        },
        zaiOauth: {
          pendingPrimary: "Waiting for Z.AI authorization",
          pendingSecondary: "Please complete the login in your browser. Configuration will continue after authorization is completed.",
          primary: "Z.AI Coding Plan",
          secondary: "Open a browser to log in and create a Coding Plan API key.",
        },
      },
      pending: {
        cancelStatus: "Login canceled. Please select a configuration method.",
        help: "Press Esc to cancel and return to configuration mode selection.",
        status: "Waiting for browser authorization...",
      },
      input: {
        cancelStatus: "API key entry canceled. Please select a configuration method.",
        clearStatus: "API key input cleared.",
        emptyStatus: "API key cannot be empty.",
        help: "Press Enter to save the key, and press Esc to return to configuration mode selection.",
        placeholder: "Paste API key",
        status: "Enter the API key and press Enter.",
        submitStatus: "Saving API key...",
      },
      prompt: "Select login or API key configuration method.",
      response: "Select how your Coding Plan provider is configured.",
      title: "Configure Coding Plan",
    },
    model: {
      requestFailed: (message) => `Model request failed: ${message}`,
      responseReceived: "Model response received.",
      responseReceivedWithTokens: (tokens) => `Model response received. ${tokens} tokens.`,
      retryScheduled: ({ attempt, delay, maxAttempts, reason }) =>
        `Model requests will be retried after ${delay} ${attempt}/${Math.max(1, maxAttempts - 1)}: ${reason}`,
      streamStalled: "Model streaming output stalled.",
    },
    sidebar: {
      subagents: {
        title: "subagent",
        empty: "No sub-agent yet",
        emptyOutput: "The subagent has not produced output yet",
        back: "← Return to main session",
        readonly: "Read only · Esc return",
        loading: "Loading subagent output...",
        unavailable: "Unable to read subagent output",
        retry: "Try again",
        more: "load more",
        pendingMain: "The main session requires your input and will be processed after returning.",
        ended: (count) => `Ended (${count})`,
        status: {
          running: "Running",
          waiting: "waiting for input",
          blocked: "block",
          success: "Completed",
          failed: "fail",
          cancelled: "Canceled",
          lost: "Lost contact",
        },
      },
      api: {
        empty: "There are no API calls yet.",
        model: "Model",
        more: (count) => `${count} more items`,
        requests: "ask",
        server: "Server",
      },
      cache: {
        hit: "hit",
        lastHit: "last hit",
        lastMiss: "Last miss",
        readWrite: ({ read, write }) => `Read ${read} / Write ${write}`,
        total: "total",
      },
      context: {
        cache: "cache",
        cacheReadWrite: "Cache read and write",
        inputOutput: "input/output",
        reason: "reasoning",
        tokens: "Tokens",
        used: "Used",
        window: "window",
      },
      modifiedFiles: {
        empty: "No file changes yet.",
        more: (count) => `There are ${count} files left`,
      },
      mcp: {
        empty: "MCP server is not configured.",
        loadFailed: "Unable to get MCP status.",
        loading: "Loading MCP status...",
        more: (count) => `There are ${count} more`,
        servers: "Serve",
        status: {
          connected: "Connected",
          connecting: "Connecting",
          disabled: "Disabled",
          disconnected: "Not connected",
          failed: "fail",
          untrusted: "not trusted",
        },
        summary: ({ connected, total }) => `${connected}/${total} Connected`,
        tools: (count) => `${count} tool`,
      },
      request: {
        complete: "Finish",
        error: "mistake",
        errorWithStatus: (statusCode) => `Error ${statusCode}`,
        pending: "in progress",
      },
      status: {
        last: "recent",
      },
      run: {
        draft: "draft",
        draftChars: (count) => `${count} characters`,
        draftEmpty: "null",
        messages: "information",
        mode: "model",
        model: "Model",
        provider: "supplier",
        thought: "think",
        trace: "Trace",
        turn: "round",
        workspace: "workspace",
      },
      sections: {
        apis: "API",
        context: "context",
        mcp: "MCP",
        modifiedFiles: "change file",
        run: "run",
        status: "state",
        todos: "Todos",
      },
      shellSubtitle: "OpenTUI terminal",
      title: "sidebar",
      todos: {
        empty: "There are no todos yet.",
        more: (count) => `There are ${count} more items`,
        progress: "schedule",
      },
    },
    status: {
      compactFailed: "Context compression failed.",
      compacted: "Conversation compressed.",
      compacting: "Compressing context...",
      interruptedStreamDiscarded: "Interrupted model flow discarded.",
      modelCalling: "Calling model...",
      permissionRequested: (toolName) => `Requesting permission for ${toolName}.`,
      permissionResolved: (toolName) => `${toolName} permissions processed.`,
      ready: "Ready.",
      recoveringStream: "Resuming interrupted model flow...",
      retryingStream: "Retrying model flow...",
      sessionResumed: "Session has been restored.",
      targetChanged: (action) => `Target ${action}.`,
      thinking: "Thinking...",
      toolCompleted: (toolName) => `Tool ${toolName} has been completed.`,
      toolFailed: (toolName) => `Tool ${toolName} failed.`,
      toolPending: (toolName) => `Tool ${toolName} is pending.`,
      toolRunning: (toolName) => `Tool ${toolName} is running.`,
      turnFailed: "This round failed.",
    },
    terminal: {
      starting: "Starting ZCode... Ctrl+C to exit",
      requiresInteractive: "TUI requires an interactive terminal.",
    },
    transcript: {
      compact: {
        completed: "Context is compressed",
        failed: "Context compression failed",
        interrupted: "Context compression interrupted",
        retry: (command) => `Ctrl-R Retry ${command}`,
        retrying: ({ attempt, maxAttempts }) =>
          maxAttempts > 0
            ? `Retrying compression context (${attempt}/${maxAttempts})`
            : "Retrying compression context",
        skipped: "The context is already up to date and does not need to be compressed",
        started: "Compressing context",
      },
      roles: {
        agent: "Agent",
        system: "System",
        user: "User",
      },
      thought: {
        complete: "think",
        thinking: "Thinking...",
      },
      title: "dialogue",
      workflow: {
        actors: "actors：",
        actorRow: ({ name, status }) => `${name} - ${status}`,
        usage: ({ spentTokens }) => `Usage: ${spentTokens} tokens`,
        collapsed: ({ label, status, nodesSettled, nodesTotal }) =>
          `Workflow ${label} - ${status} (${nodesSettled}/${nodesTotal} steps)`,
        error: (message) => `Error: ${message}`,
        expandHint: "+ expand",
        collapseHint: "- close",
        log: "log:",
        nodes: ({ nodesSettled, nodesTotal }) => `${nodesSettled}/${nodesTotal} steps settled`,
        result: (preview) => `Result: ${preview}`,
        status: {
          completed: "Completed",
          errored: "Error",
          pending: "To be started",
          running: "Running",
          stopped: "Stopped",
        },
        stopReason: {
          user: "you stopped",
          model: "Agent stopped",
          provider: "Model side error",
          interrupted: "Process has exited",
          superseded: "Replaced by adjusted run",
        },
        truncated: "(truncated - full facts in run's event log)",
        interruptedNotice: ({ label, runId }) =>
          `Workflow ${label} was interrupted and can be resumed: /dwf resume ${runId}`,
      },
    },
    selection: {
      defaultHelp: "Enter to select, Esc to cancel",
      disabled: (reason) => `[Disabled: ${reason}]`,
      filterLine: ({ filter, help }) =>
        `Filter: ${filter || "-"} | ${help ?? "Enter to select, Esc to cancel"}`,
      noFilter: "-",
    },
    fileMention: {
      empty: "No matching workspace path.",
      loading: "Reading workspace path...",
      row: ({ path, selected }) => `${selected ? ">" : " "} ${path}`,
      title: "document",
    },
    slash: {
      title: "Order",
      row: ({ name, selected, summary }) => `${selected ? ">" : " "} /${name}  ${summary}`,
    },
  },
};
