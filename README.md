# niri-mapper

Input remapping daemon for niri (Wayland compositor).

## Features

- Simple 1:1 key remapping (e.g., CapsLock to Escape)
- KDL configuration format
- NixOS and Home Manager module support
- Systemd integration

## Finding Your Keyboard Device Name

Before configuring niri-mapper, you need to find the exact name of your keyboard device:

```bash
# Using niri-mapper CLI
niri-mapper devices

# Or using evtest (requires sudo)
sudo evtest
```

Device names are case-sensitive. Copy the exact name as shown.

Common device names:
- `AT Translated Set 2 keyboard` - Built-in laptop keyboard
- `Keychron K3 Pro` - External USB keyboard
- `HHKB-Hybrid Keyboard` - Happy Hacking Keyboard

## NixOS Usage

### 1. Add the flake input

In your `flake.nix`:

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    niri-mapper = {
      url = "github:barrulus/niri-mapper";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, niri-mapper, ... }: {
    nixosConfigurations.yourhostname = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        ./configuration.nix
        niri-mapper.nixosModules.default
      ];
    };
  };
}
```

### 2. Minimal Configuration

Add to your `configuration.nix`:

```nix
{ config, pkgs, ... }:

{
  # Required: add your user to the input group
  users.users.yourusername.extraGroups = [ "input" ];

  services.niri-mapper = {
    enable = true;
    settings.devices = [{
      name = "Your Keyboard Name";  # Run `niri-mapper devices` to find this
      profiles.default.remap.CapsLock = "Escape";
    }];
  };
}
```

### 3. Apply Configuration

```bash
sudo nixos-rebuild switch --flake .#yourhostname
```

## Home Manager Usage

### 1. Add the flake input (same as NixOS)

### 2. Import the module

In your Home Manager configuration:

```nix
{ config, pkgs, inputs, ... }:

{
  imports = [
    inputs.niri-mapper.homeManagerModules.default
  ];

  services.niri-mapper = {
    enable = true;
    settings.devices = [{
      name = "Your Keyboard Name";
      profiles.default.remap.CapsLock = "Escape";
    }];
  };
}
```

**Note**: For Home Manager, you must ensure your user is in the `input` group separately (either via NixOS configuration or manually).

## Common Remapping Examples

### Swap CapsLock and Escape

```nix
services.niri-mapper.settings.devices = [{
  name = "Your Keyboard Name";
  profiles.default.remap = {
    CapsLock = "Escape";
    Escape = "CapsLock";
  };
}];
```

### CapsLock to Control

```nix
services.niri-mapper.settings.devices = [{
  name = "Your Keyboard Name";
  profiles.default.remap.CapsLock = "LeftCtrl";
}];
```

### Multiple Remappings

```nix
services.niri-mapper.settings.devices = [{
  name = "Your Keyboard Name";
  profiles.default.remap = {
    CapsLock = "Escape";
    LeftAlt = "LeftCtrl";
    LeftCtrl = "LeftAlt";
  };
}];
```

## CLI Commands

```bash
# List available input devices
niri-mapper devices

# Validate configuration
niri-mapper validate

# Validate and check device matching
niri-mapper validate --dry-run

# Service management (via systemd)
niri-mapper start
niri-mapper stop
niri-mapper status
```

## Troubleshooting

### Service won't start

1. Check if you're in the `input` group:
   ```bash
   groups | grep input
   ```
   If not, add yourself and log out/in.

2. Check if uinput module is loaded:
   ```bash
   lsmod | grep uinput
   ```
   The NixOS module loads this automatically.

3. Check service logs:
   ```bash
   journalctl -u niri-mapper -f
   ```

### Device not found

1. Verify the exact device name:
   ```bash
   niri-mapper devices
   ```

2. Device names are case-sensitive - copy exactly as shown.

### Remapping not working

1. Verify the service is running:
   ```bash
   systemctl status niri-mapper
   ```

2. Check that the device was grabbed:
   ```bash
   journalctl -u niri-mapper | grep -i grab
   ```

## License

GPL-3.0-or-later
