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

## Manual Installation (non-NixOS)

For Arch, Fedora, Ubuntu, and other distributions:

### 1. Build and install

```bash
cargo build --release
sudo cp target/release/niri-mapper /usr/bin/
sudo cp target/release/niri-mapperd /usr/bin/
```

### 2. Add user to input group

```bash
sudo usermod -aG input $USER
# Log out and back in for group change to take effect
```

### 3. Create configuration

```bash
mkdir -p ~/.config/niri-mapper
cat > ~/.config/niri-mapper/config.kdl << 'EOF'
device "Your Keyboard Name" {
    profile "default" {
        remap {
            CapsLock "Escape"
        }
    }
}
EOF
```

Run `niri-mapper devices` to find your keyboard's exact name.

### 4. Install and enable systemd service

```bash
mkdir -p ~/.config/systemd/user
cp systemd/niri-mapper.user.service ~/.config/systemd/user/niri-mapper.service
systemctl --user daemon-reload
systemctl --user enable --now niri-mapper
```

### 5. Verify

```bash
systemctl --user status niri-mapper
```

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

## Manual Testing

Follow this procedure to verify the daemon is working correctly:

### 1. Create a Test Configuration

Create `~/.config/niri-mapper/config.kdl`:

```kdl
device "Your Keyboard Name" {
    profile "default" {
        remap {
            CapsLock "Escape"
        }
    }
}
```

Replace `"Your Keyboard Name"` with your actual keyboard name from `niri-mapper devices`.

### 2. Start the Daemon

```bash
# Start in foreground for testing (logs visible in terminal)
niri-mapperd --config ~/.config/niri-mapper/config.kdl --foreground
```

Or via systemd:

```bash
systemctl --user start niri-mapper
```

### 3. Verify Device Grab

Check that the daemon grabbed your keyboard:

```bash
# Check service logs
journalctl --user -u niri-mapper -f

# Or check dmesg for uinput device creation
sudo dmesg | tail -20
```

You should see messages like:
- `Matched device 'Your Keyboard Name' at /dev/input/eventX`
- `Grabbed device: Your Keyboard Name`

### 4. Test Key Remapping

1. Open a text editor or terminal
2. Press CapsLock - it should produce Escape (close dialogs, etc.)
3. Verify CapsLock LED does NOT toggle (the key is being remapped)

### 5. Verify Graceful Shutdown

Stop the daemon and verify the device is released:

```bash
# Stop the daemon
systemctl --user stop niri-mapper

# Or if running in foreground, press Ctrl+C
```

Check logs for clean shutdown:
```bash
journalctl --user -u niri-mapper | tail -10
```

You should see:
- `Received shutdown signal`
- `Released device: Your Keyboard Name`

After stopping, your physical keyboard should work normally again without remapping.

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
