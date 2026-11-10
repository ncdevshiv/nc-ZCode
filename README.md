# ZCode

<div align="center">
  <img src="public/logo/icons/1024x1024.png" alt="ZCode" width="128" height="128" />
</div>
<p align="center">
<a href="https://applink.feishu.cn/client/chat/chatter/add_by_link?link_token=47ag983c-8fcb-4d6d-814b-5395193a712c&qr_code=true">Feishu Community</a> ·
  <a href="https://discord.gg/z9aBcQXZQ3">Discord</a>
</p>
<p align="center">
Simplified Chinese | <a href="README.en.md">English</a>
</p>

ZCode is an AI programming workbench that provides desktop applications, browser interfaces and terminal Agents. This repository contains client, backend services, shared UI, Agent CLI and runtime source code.

| Entrance | Purpose | Development Command |
| -------------------- | -------------------------------------------------------------- | ------------------------------ |
| Desktop | Tauri desktop application (Electron fallback) | `bun run dev:desktop` (→ `dev:tauri`) |
| Web / ZCode command line version | Terminal and browser workbench; assembles TUI, Web, backend and Agent into independent running packages | `bun run dev:web` |
| Agent CLI | Use `zcode` in the terminal, also provides Agent runtime for Desktop and Web | `bun run --filter @zcode/cli dev` |

## Initialization

Prepare Git, Node.js **24.14.0** and Bun **1.4.3**, the version is subject to [mise.toml](mise.toml). The following development and packaging commands are executed in the warehouse root directory.

```bash
bun run bootstrap
```

`bun run bootstrap` installs workspace dependencies, prepares desktop local running resources, and then executes `build:bootstrap`.

The Agent CLI and runtime source code are located in [apps/zcode-cli/](apps/zcode-cli/) and are cloned together with this repository as a normal directory. There is no need to pull or initialize the Git submodule separately.

Choose other initialization or build entries as needed:

| Command | Purpose |
| ------------------------------ | ----------------------------------------------------------------- |
| `bun install` | Install dependencies |
| `bun run prepare:desktop-runtime` | Prepare desktop running resources, including remote resource preparation by default |
| `bun run prepare:remote-assets` | Prepare remote running resources individually |
| `bun run bootstrap:with-remote` | Initialize dependencies, local and remote resources, and build related packages serially; skip desktop application bundles |
| `bun run build` | Recursively execute the build script of each workspace package, including the resource preparation steps in the package |

By default `bootstrap` skips remote resource preparation and is suitable for local desktop development. When using a remote workspace or verifying remote release resources, run the corresponding preparation command.

## Development and operation

### Desktop version

```bash
bun run dev:desktop

# Use test environment
bun run dev:desktop:test
```

`bun run dev:desktop` aliases `bun run dev:tauri` (`package.json:15`): it builds `@zcode/web` and runs `cargo run --manifest-path native/Cargo.toml -p nc-code-shell` (Tauri shell + embedded Rust backend). Electron is the fallback via `bun run dev:desktop:prod` / `dev:desktop:test` (`scripts/dev-desktop-env.mjs`), which prepare runtime assets, build the desktop agent, and start Electron with source watchers.

When you need to develop the data directory independently, you can set `ZCODE_DATA_BASE_DIR`. For example in macOS/Linux:

```bash
ZCODE_DATA_BASE_DIR="$HOME/.zcode-dev-home" bun run dev:desktop:test
```

### Remote function (SSH/WSL)

First execute `bun run bootstrap:with-remote` to prepare the remote resource (mock-cdn), and then `bun run dev:desktop`; when connecting to the remote project, select "Local download and then upload" for the resource. Development resources are taken from the local `packages/desktop/mock-cdn` and local build products, and are uploaded to the remote via SFTP without accessing the CDN.

### Web Development

When modifying web or backend source code, use development mode:

```bash
bun run dev:web

#Specify backend workspace (macOS/Linux)
ZCODE_SERVER_WORKSPACE=/path/to/project bun run dev:web
```

This command starts both the web development server (default `http://localhost:5173`) and the backend (default `http://localhost:3030`); the browser accesses the former. `/ws` and general `/api` requests are proxied to the local backend, and `/api/v1/oauth/token` alone is proxied to the currently configured product service.

After modifying the Agent source code, execute `bun run --filter @zcode/cli... build` and restart the service. When you need to verify the complete distribution package, press the "ZCode Command Line Version" packaging chapter below to decompress and run it.

### ZCode command line version

The command line distribution package includes TUI, Web and Agent, which are all started using `zcode`: enter TUI without parameters; start Web when the first parameter is `--web`; other parameters are handled by the existing Agent CLI. Both modes run natively, without the need for Electron.

```bash
# Enter the terminal interactive interface by default
zcode

# Start the web interface
zcode --web

# Specify the project and port and do not automatically open the browser
zcode --web --workspace /path/to/project --port 3030 --no-open

# View CLI or web parameters
zcode --help
zcode --web --help
```

The default working directory in Web mode is the current directory, listening on `127.0.0.1`, access tokens are not enabled by default, an idle port is automatically selected and the browser is opened. Access the address output by the terminal and press `Ctrl+C` to stop the service. `--host 0.0.0.0` can be used for LAN access; an access token is generated by default when listening to a non-local address, and the tokenized link output by the terminal is used. Token authentication can be turned off by specifying a token with `--token` or turning off `--no-token`.

When directly starting the HTTP entry of a general web service, configure API/WebSocket authentication through `ZCODE_SERVER_AUTH_TOKEN`; when creating a service through the program interface, use the `authToken` option.

See the packaging chapter below for construction methods. `bun run build:zcode` only generates distribution packages and does not replace existing `zcode` in `PATH`. If the command still points to an old installation or other source directory, check with `command -v zcode` on macOS/Linux and `where.exe zcode` on Windows.

### CLI source code development

When developing TUI or Agent directly, run the source code entry:

```bash
bun run --filter @zcode/cli dev --help
bun run --filter @zcode/cli dev

# Build CLI and its workspace dependencies
bun run --filter @zcode/cli... build
node apps/zcode-cli/packages/cli/dist/zcode.cjs --help
```

This entry runs the Agent CLI directly without going through the `--web` branch of the distribution package. Use `bun run dev:web` to develop the Web; to verify the unified `zcode` command, use the unzipped `bin/zcode.mjs` below.

## Configuration

The root directory [.env.example](.env.example) provides service addresses and build configuration examples, which can be copied to `.env` as needed, and local overlays are placed in `.env.local`. The Desktop development environment is selected through `dev:desktop:test` / `dev:desktop:prod`.

| Configuration | Usage |
| ------------------------------------ | ------------------------------------------------ |
| `ZCODE_DATA_BASE_DIR` | Application data base directory, data is written to `.zcode/` under it |
| `ZCODE_SERVER_WORKSPACE` | Workspace path for the web backend |
| `ZCODE_BUILTIN_PROVIDER_CONFIG_FILE` | Local Provider configuration file path; if not set, the built-in configuration will be used |
| `ZCODE_DIST_BASE_URL` | The download root address used by the command line installation script |

Runtime variables can be set explicitly in the environment in which the command is launched. The default configuration shipped with the client is found in [config/README.md](config/README.md).

## Pack

For the third-party statement generation, release verification process and the position of the statement in the release, see [third-party/README.md](third-party/README.md).

### Desktop version

```bash
bun run bundle:desktop

#Specify target platform and CPU architecture
bun run bundle:desktop -- --os win --arch x64

bun run bundle:desktop -- --help
```

The default target is macOS arm64 and the default output directory is `packages/desktop/dist/`. `--os` supports `mac`, `win`, `linux`, `--arch` supports `x64`, `arm64`; actual packaging and signing require tools and configuration corresponding to the target platform.

Installation: Double-click to open the product DMG and drag ZCode into "Applications". The local build is not signed. If it is intercepted by macOS when opened for the first time, execute:

```bash
sudo xattr -rd com.apple.quarantine /Applications/ZCode.app
```

### ZCode command line version

The build entry point is `bun run build:zcode`. The script will build CLI/TUI, backend and Web in sequence, collect TUI's native libraries, workers and runtime dependencies, and then assemble the distribution package; Node.js is still required to run the distribution package, and the version is subject to `mise.toml`.

The download root address `ZCODE_DIST_BASE_URL` must be set before packaging (can be placed in `.env`, `.env.local` or environment variables), or it can be passed in through `--base-url`. The following addresses are placeholder examples and were replaced with actual hosting addresses when published:

```bash
bun run build:zcode --base-url https://downloads.example.com/zcode/

# When ZCODE_DIST_BASE_URL is configured
bun run build:zcode

# Only reassemble the package and reuse the existing Agent, backend and Web build products
bun run build:zcode --skip-build

# View optional parameters such as version and output directory
bun run build:zcode --help
```

The default version takes the root directory `package.json`, and the output directory is `dist/zcode/`:

- `releases/<version>/zcode-<version>.tar.gz`: Run the package.
- `releases/<version>/sha256.txt`: Verification summary.
- `latest.json`, `install.sh`: version index and installation script.

The complete catalog can be uploaded to the configured download root address. The installation script downloads the runtime package from this address, installs it to `~/.zcode/runtime` by default, and creates the `zcode` command in `~/.local/bin`. The installation directory can be modified through `ZCODE_DIST_HOME`, and the command directory can be modified through `ZCODE_DIST_BIN_DIR`.

Old Lite users will need to use the above build commands, environment variables, and new installation scripts instead. A new installation will not delete the old Lite directory, nor will existing session data be migrated or deleted.

When debugging packaged products locally, you can directly decompress and run them without uploading or installing:

```bash
zcode_version=$(node -p "require('./dist/zcode/latest.json').version")
mkdir -p dist/zcode/debug
tar -xzf "dist/zcode/releases/$zcode_version/zcode-$zcode_version.tar.gz" \
  -C dist/zcode/debug
# Start TUI by default
node dist/zcode/debug/zcode/bin/zcode.mjs

# Start Web
node dist/zcode/debug/zcode/bin/zcode.mjs --web \
  --workspace "$PWD" --port 3030 --no-open
```

Open `http://127.0.0.1:3030` in the browser to verify the complete link of the web page and Agent hosted by the same backend service. This port needs to be free; if you are running `bun run dev:web`, you can use another `--port` instead.

## Warehouse structure

| Table of Contents | Responsibilities |
| ---------------------------------------------------- | ------------------------------------------ |
| `packages/desktop` | Tauri shell (Electron fallback) Main, Host, Renderer and desktop packaging |
| `packages/web` | Web client |
| `packages/server` | HTTP/WebSocket services and remote connections |
| `packages/zcode-server-cli` | Independent Server startup and process management |
| `packages/ui` | Shared React components, hooks and Zustand state |
| `packages/services` | Business services and persistence |
| `packages/shared`, `packages/rpc`, `packages/client` | Shared protocols and types, RPC framework, Agent client SDK |
| `packages/provider`, `packages/provider-node` | Provider public capabilities and Node implementation |
| `apps/zcode-cli` | Agent CLI, TUI, runtime and tools |
| `scripts`, `config`, `third-party` | Build and maintain scripts, built-in configuration and third-party declaration materials |

## Project Statement

For functionality and offer scope, maintenance rules, execution and data risks, as well as license and third-party copyright notices, see [NOTICE.md](NOTICE.md).
