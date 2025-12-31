# Niri Mapper - Product Requirements Document

## Overview

Niri Mapper is a Rust-based input remapping daemon designed for Wayland, with first-class NixOS support and deep integration with the [niri](https://github.com/YaLTeR/niri) compositor. It enables hardware-level input remapping while maintaining clean separation from compositor keybinds through niri's KDL include system.

## Problem Statement

Existing input remapping solutions have limitations:

1. **input-remapper** (Python) - Works well but lacks native niri integration and NixOS-first design
2. **keyd** - C-based, X11-centric, no Wayland-native design
3. **xremap** - Good Rust implementation but focused on application-aware remapping rather than hardware-level

Users of niri on NixOS need a solution that:
- Handles hardware-level remapping (macros, key combinations, device-specific profiles)
- Integrates cleanly with niri's keybind system to avoid conflicts
- Works declaratively within NixOS configuration
- Is maintainable, type-safe, and performant (Rust)

## Goals

### Primary Goals

1. **Hardware Input Remapping** - Remap keys, buttons, and axes at the evdev level
2. **Niri Integration** - Generate niri-compatible KDL config fragments to prevent keybind conflicts
3. **NixOS Native** - Provide a flake with NixOS/home-manager modules
4. **Wayland Native** - No X11 dependencies; designed for modern Wayland compositors

### Secondary Goals

1. Support other Linux distributions via standard packaging
2. Provide a TUI for configuration (no GUI initially)
3. Support device hot-plugging
4. Per-application context switching (future)

## Non-Goals

- GUI application (out of scope for v1)
- X11 support
- Windows/macOS support
- Gaming-focused features (anti-cheat bypass, etc.)

## Architecture

### System Components

```
┌─────────────────────────────────────────────────────────────┐
│                      User Space                             │
│  ┌─────────────┐    ┌──────────────┐    ┌───────────────┐  │
│  │ niri-mapper │───▶│ Config Files │◀───│ NixOS Module  │  │
│  │   daemon    │    │    (KDL)     │    │ (flake.nix)   │  │
│  └──────┬──────┘    └──────────────┘    └───────────────┘  │
│         │                                                   │
│         │ generates                                         │
│         ▼                                                   │
│  ┌──────────────┐         ┌─────────────────────────────┐  │
│  │ niri-keybinds│────────▶│ niri config.kdl             │  │
│  │    .kdl      │ include │ include "niri-keybinds.kdl" │  │
│  └──────────────┘         └─────────────────────────────┘  │
│                                                             │
└─────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────┐
│                      Kernel Space                           │
│  ┌─────────────┐    ┌──────────────┐    ┌───────────────┐  │
│  │   evdev     │───▶│  niri-mapper │───▶│    uinput     │  │
│  │  (grabbed)  │    │  (intercept) │    │  (virtual)    │  │
│  └─────────────┘    └──────────────┘    └───────────────┘  │
└─────────────────────────────────────────────────────────────┘
```

### Core Modules

| Module | Responsibility |
|--------|----------------|
| `niri-mapper-daemon` | Main service that grabs devices and processes events |
| `niri-mapper-cli` | CLI tool for control, status, and config validation |
| `niri-mapper-config` | Configuration parsing (KDL) and validation |
| `niri-mapper-nix` | NixOS module and home-manager integration |

### Technology Stack

- **Language**: Rust (2021 edition)
- **Async Runtime**: tokio
- **Input Handling**: evdev-rs, uinput
- **Configuration**: KDL (kdl-rs)
- **Compositor IPC**: niri IPC (Unix socket)
- **Logging**: tracing
- **CLI**: clap
- **NixOS**: Flake with module

## Configuration Design

### Primary Config (KDL)

```kdl
// ~/.config/niri-mapper/config.kdl

global {
    log-level "info"
    niri-keybinds-path "~/.config/niri/niri-mapper-keybinds.kdl"
}

// Device matching by name, vendor:product, or path
device "Keychron K3 Pro" {
    // vendor-product "3434:0361"  // Alternative matching

    profile "default" {
        // Simple key remapping
        remap {
            CapsLock "Escape"
            Escape "CapsLock"
        }

        // Key combinations
        combo {
            Ctrl+Shift+Q "Alt+F4"
        }

        // Macros (sequential key presses)
        macro {
            Ctrl+Shift+D "Ctrl+c" "delay(50)" "Ctrl+v"
        }

        // Keys to pass through to niri (generates niri keybinds KDL)
        // Single source of truth: define the action here, niri-mapper generates niri config
        niri-passthrough {
            Super+Return { spawn "alacritty"; }
            Super+d { spawn "fuzzel"; }
            Super+q { close-window; }
            Super+1 { focus-workspace 1; }
            Super+2 { focus-workspace 2; }
        }
    }
}

device "Logitech G Pro" {
    profile "default" {
        remap {
            XF86Back "Alt+Left"
            XF86Forward "Alt+Right"
        }
    }
}
```

### Generated Niri Keybinds KDL

niri-mapper generates a KDL file from the `niri-passthrough` blocks for inclusion in your niri config:

```kdl
// Auto-generated by niri-mapper - DO NOT EDIT
// Source: ~/.config/niri-mapper/config.kdl
// Include this in your niri config.kdl:
//   include "~/.config/niri/niri-mapper-keybinds.kdl"

binds {
    Mod+Return { spawn "alacritty"; }
    Mod+d { spawn "fuzzel"; }
    Mod+q { close-window; }
    Mod+1 { focus-workspace 1; }
    Mod+2 { focus-workspace 2; }
}
```

This approach provides a **single source of truth**: all keybinds (both remapped and passthrough) are defined in niri-mapper's config, eliminating sync issues between the two configs.

## Niri Integration Strategy

### The Conflict Problem

When niri-mapper grabs a keyboard, it intercepts ALL key events before niri sees them. This means niri keybinds won't work unless niri-mapper explicitly passes them through.

### The Solution

1. **Passthrough Declaration**: Users declare which keys niri should handle in the niri-mapper config
2. **KDL Generation**: niri-mapper generates a KDL file with those bindings
3. **Include in Niri**: User adds `include "path/to/generated.kdl"` to their niri config
4. **Event Forwarding**: niri-mapper forwards passthrough keys unmodified to the virtual device

### Workflow

```
User edits niri-mapper config.kdl
        ↓
niri-mapper validates and regenerates niri-keybinds.kdl
        ↓
User reloads niri config (or niri watches for changes)
        ↓
Bindings are synchronized
```

## NixOS Integration

### Flake Structure

```
niri-mapper/
├── flake.nix
├── flake.lock
├── Cargo.toml
├── Cargo.lock
├── src/
├── nix/
│   ├── module.nix          # NixOS module
│   ├── hm-module.nix       # home-manager module
│   └── package.nix         # Package derivation
└── ...
```

### NixOS Module Usage

```nix
# flake.nix
{
  inputs.niri-mapper.url = "github:barrulus/niri-mapper";
}

# configuration.nix
{ inputs, ... }: {
  imports = [ inputs.niri-mapper.nixosModules.default ];

  services.niri-mapper = {
    enable = true;
    settings = {
      devices = [{
        name = "Keychron K3 Pro";
        profile.default = {
          remap = {
            CapsLock = "Escape";
          };
          niri_passthrough.keys = [
            "Super+Return"
            "Super+d"
          ];
        };
      }];
    };
  };
}
```

### Home-Manager Module Usage

```nix
{ inputs, ... }: {
  imports = [ inputs.niri-mapper.homeManagerModules.default ];

  services.niri-mapper = {
    enable = true;
    # Same settings structure as NixOS module
  };
}
```

## Feature Roadmap

### v0.1.0 - MVP

- [ ] Basic daemon with evdev grab and uinput injection
- [ ] Simple key remapping (1:1)
- [ ] KDL configuration parsing
- [ ] Device matching by name
- [ ] Systemd service unit
- [ ] Basic CLI (start, stop, status)
- [ ] NixOS flake with package

### v0.2.0 - Niri Integration

- [ ] KDL generation for niri passthrough keys
- [ ] Key combination remapping
- [ ] NixOS module
- [ ] home-manager module
- [ ] Config hot-reload (SIGHUP)

### v0.3.0 - Advanced Features

- [ ] Macro support (key sequences with delays)
- [ ] Device hot-plug support
- [ ] Multiple profiles per device
- [ ] TUI configuration tool
- [ ] Vendor:product device matching

### v0.4.0 - Niri IPC Integration

- [ ] Connect to niri IPC socket
- [ ] Query active window/workspace info
- [ ] React to focus change events
- [ ] Foundation for per-application profiles (backlog)

### v0.5.0 - Polish

- [ ] Axis remapping (mouse, gamepad)
- [ ] Documentation and examples
- [ ] Other distro packaging (Arch AUR, etc.)

### Backlog

- [ ] Automatic/contextual profile switching (based on focused app)
- [ ] Multi-seat support
- [ ] GUI configuration tool

## Technical Requirements

### Performance

- Event latency: <1ms additional latency
- Memory: <10MB RSS
- CPU: Negligible when idle

### Reliability

- Graceful handling of device disconnect
- Clean shutdown on SIGTERM
- Automatic device re-grab on reconnect
- Config validation before applying

### Security

- Minimal required capabilities (CAP_SYS_ADMIN for uinput)
- No network access required
- Sandboxable via systemd

## Dependencies

### Runtime

- Linux kernel with evdev and uinput support
- systemd (for service management)
- niri (optional, for integration features)

### Build

- Rust toolchain (stable)
- pkg-config
- libudev-dev (for device enumeration)

## Success Metrics

1. **Functionality**: All input-remapper features used by target users work
2. **Integration**: Zero conflicts between niri-mapper and niri keybinds when configured correctly
3. **NixOS Experience**: Single-line enable in NixOS config
4. **Performance**: No perceptible input latency
5. **Stability**: No crashes or hangs in normal operation

## Design Decisions

1. **Profile Switching**: Manual trigger only for v1. Automatic/contextual switching is backlog.
2. **Niri IPC**: Yes - use niri's IPC for window focus events, workspace changes, and tighter integration.
3. **Multi-seat**: Not supported. Single-user only for v1.
4. **Niri Keybind Actions**: Inline in niri-mapper config. Single source of truth - niri-mapper config defines both the passthrough keys AND the niri actions, then generates the niri KDL.

## References

- [input-remapper](https://github.com/sezanzeb/input-remapper) - Primary inspiration
- [niri](https://github.com/YaLTeR/niri) - Target compositor
- [evdev-rs](https://github.com/ndesh26/evdev-rs) - Rust evdev bindings
- [KDL](https://kdl.dev/) - Configuration language used by niri
