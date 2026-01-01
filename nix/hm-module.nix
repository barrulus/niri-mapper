# Home Manager module for niri-mapper
#
# ============================================================================
# OWNERSHIP BOUNDARY: NixOS Module vs Home-Manager Module
# ============================================================================
#
# Use this decision guide to choose the right module for your setup:
#
# HOME-MANAGER MODULE (this file) - Use when:
#   - Running niri-mapper as your regular user (recommended for most users)
#   - Managing user-specific configuration in your home-manager config
#   - You want per-user keybind customization
#
# NIXOS MODULE (module.nix) - Use when:
#   - Running niri-mapper as a system service (requires root or input group)
#   - Managing system-wide configuration
#   - Setting up the initial system prerequisites (udev rules, kernel modules)
#
# ============================================================================
# WHAT EACH MODULE OWNS
# ============================================================================
#
# NIXOS MODULE owns (system-level, requires root):
#   - udev rules (/etc/udev/rules.d/) - Grants access to /dev/uinput
#   - Kernel module loading (boot.kernelModules = [ "uinput" ])
#   - System-wide systemd service (if running as root)
#   - Input group membership via users.users.<name>.extraGroups = [ "input" ]
#
# HOME-MANAGER MODULE owns (user-level):
#   - User config file (~/.config/niri-mapper/config.kdl)
#   - User systemd service (~/.config/systemd/user/niri-mapper.service)
#   - Generated niri keybinds (~/.config/niri/niri-mapper-keybinds.kdl)
#
# ============================================================================
# IMPORTANT: PREREQUISITES FOR HOME-MANAGER USAGE
# ============================================================================
#
# Even when using the home-manager module, you MUST have NixOS-level setup:
#
# 1. Enable udev rules (in your NixOS configuration.nix):
#
#    services.udev.extraRules = ''
#      KERNEL=="uinput", MODE="0660", GROUP="input", OPTIONS+="static_node=uinput"
#    '';
#
# 2. Load the uinput kernel module:
#
#    boot.kernelModules = [ "uinput" ];
#
# 3. Add your user to the input group:
#
#    users.users.<your-username>.extraGroups = [ "input" ];
#
# Without these, the home-manager service will fail to access input devices.
#
# ============================================================================
# WARNING: MUTUAL EXCLUSIVITY
# ============================================================================
#
# Do NOT enable both the NixOS system service AND the home-manager user service
# simultaneously for the same user. They will conflict when trying to grab the
# same input devices. Choose one:
#
#   - NixOS module with services.niri-mapper.enable = true (system service)
#   - Home-manager module with services.niri-mapper.enable = true (user service)
#
# If you need NixOS for udev rules but want a user service, enable NixOS module
# options for udev/kernel modules only, without enabling the system service:
#
#   # NixOS config - just the prerequisites
#   boot.kernelModules = [ "uinput" ];
#   services.udev.extraRules = ''...'';
#   users.users.myuser.extraGroups = [ "input" ];
#
#   # Home-manager config - the actual service
#   services.niri-mapper.enable = true;
#   services.niri-mapper.settings = { ... };
#
# ============================================================================
self:
{ config, lib, pkgs, ... }:

let
  cfg = config.services.niri-mapper;

  # Type definitions for niri-mapper configuration (mirrored from NixOS module)

  # Global settings submodule
  globalType = lib.types.submodule {
    options = {
      logLevel = lib.mkOption {
        type = lib.types.enum [ "trace" "debug" "info" "warn" "error" ];
        default = "info";
        description = "Log level for the niri-mapper daemon";
      };

      niriKeybindsPath = lib.mkOption {
        type = lib.types.str;
        default = "${config.xdg.configHome}/niri/niri-mapper-keybinds.kdl";
        description = "Path to write generated niri keybinds file";
      };
    };
  };

  # Niri passthrough binding submodule
  passthroughType = lib.types.submodule {
    options = {
      key = lib.mkOption {
        type = lib.types.str;
        description = "Key combination (e.g., 'Super+Return')";
        example = "Super+Return";
      };

      action = lib.mkOption {
        type = lib.types.str;
        description = "Niri action to execute (e.g., 'spawn \"alacritty\";')";
        example = ''spawn "alacritty";'';
      };
    };
  };

  # Profile submodule
  profileType = lib.types.submodule {
    options = {
      appIdHint = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = ''
          Application ID hint for automatic profile switching.
          When set, this profile will be automatically activated when the
          focused window matches this app-id pattern (e.g., "firefox", "org.mozilla.firefox").
        '';
        example = "org.mozilla.firefox";
      };

      remap = lib.mkOption {
        type = lib.types.attrsOf lib.types.str;
        default = {};
        description = "Key remappings (from -> to)";
        example = lib.literalExpression ''
          {
            CapsLock = "Escape";
            Escape = "CapsLock";
          }
        '';
      };

      niriPassthrough = lib.mkOption {
        type = lib.types.listOf passthroughType;
        default = [];
        description = "Keybinds to pass through to niri";
        example = lib.literalExpression ''
          [
            { key = "Super+Return"; action = "spawn \"alacritty\";"; }
          ]
        '';
      };

      combo = lib.mkOption {
        type = lib.types.attrsOf lib.types.str;
        default = {};
        description = "Key combination remappings (e.g., Ctrl+Shift+Q -> Alt+F4)";
        example = lib.literalExpression ''
          {
            "Ctrl+Shift+Q" = "Alt+F4";
          }
        '';
      };
    };
  };

  # Device submodule
  deviceType = lib.types.submodule {
    options = {
      name = lib.mkOption {
        type = lib.types.str;
        description = "Device name to match (exact match)";
        example = "Keychron K3 Pro";
      };

      profiles = lib.mkOption {
        type = lib.types.attrsOf profileType;
        default = { default = {}; };
        description = "Named profiles for this device";
        example = lib.literalExpression ''
          {
            default = {
              remap = { CapsLock = "Escape"; };
            };
          }
        '';
      };

      profileSwitch = lib.mkOption {
        type = lib.types.attrsOf lib.types.str;
        default = {};
        description = ''
          Keybind-to-profile mappings for switching between profiles.
          Keys are key combinations (e.g., "Ctrl+Shift+1"), values are profile names.
          The profile names must match profiles defined in the profiles attribute.
        '';
        example = lib.literalExpression ''
          {
            "Ctrl+Shift+1" = "default";
            "Ctrl+Shift+2" = "gaming";
          }
        '';
      };
    };
  };

  # Settings submodule (top-level)
  settingsType = lib.types.submodule {
    options = {
      global = lib.mkOption {
        type = globalType;
        default = {};
        description = "Global settings for niri-mapper";
      };

      devices = lib.mkOption {
        type = lib.types.listOf deviceType;
        default = [];
        description = "List of devices to configure";
      };
    };
  };

  # Convert Nix attrset to KDL configuration
  configToKdl = settings: let
    indent = level: lib.concatStrings (lib.genList (_: "    ") level);

    globalToKdl = global: ''
      global {
          log-level "${global.logLevel or "info"}"
          niri-keybinds-path "${global.niriKeybindsPath or "${config.xdg.configHome}/niri/niri-mapper-keybinds.kdl"}"
      }
    '';

    remapToKdl = level: remap:
      lib.concatStringsSep "\n" (lib.mapAttrsToList (from: to:
        "${indent level}${from} \"${to}\""
      ) remap);

    comboToKdl = level: combo:
      lib.concatStringsSep "\n" (lib.mapAttrsToList (from: to:
        "${indent level}\"${from}\" \"${to}\""
      ) combo);

    passthroughToKdl = level: passthrough:
      lib.concatStringsSep "\n" (map (bind:
        "${indent level}${bind.key} { ${bind.action} }"
      ) passthrough);

    profileToKdl = level: name: profile: ''
      ${indent level}profile "${name}" {
      ${lib.optionalString (profile.appIdHint or null != null) ''
      ${indent (level + 1)}app-id-hint "${profile.appIdHint}"
      ''}${lib.optionalString (profile.remap or {} != {}) ''
      ${indent (level + 1)}remap {
      ${remapToKdl (level + 2) profile.remap}
      ${indent (level + 1)}}
      ''}${lib.optionalString (profile.combo or {} != {}) ''
      ${indent (level + 1)}combo {
      ${comboToKdl (level + 2) profile.combo}
      ${indent (level + 1)}}
      ''}${lib.optionalString ((profile.niriPassthrough or []) != []) ''
      ${indent (level + 1)}niri-passthrough {
      ${passthroughToKdl (level + 2) profile.niriPassthrough}
      ${indent (level + 1)}}
      ''}${indent level}}
    '';

    profileSwitchToKdl = level: profileSwitch:
      lib.concatStringsSep "\n" (lib.mapAttrsToList (keybind: profileName:
        "${indent level}\"${keybind}\" \"${profileName}\""
      ) profileSwitch);

    deviceToKdl = device: ''
      device "${device.name}" {
      ${lib.optionalString ((device.profileSwitch or {}) != {}) ''
      ${indent 1}profile-switch {
      ${profileSwitchToKdl 2 device.profileSwitch}
      ${indent 1}}
      ''}${lib.concatStringsSep "\n" (lib.mapAttrsToList (profileToKdl 1) (device.profiles or { default = {}; }))}
      }
    '';
  in ''
    // Generated by Home Manager niri-mapper module
    ${globalToKdl (cfg.settings.global or {})}

    ${lib.concatStringsSep "\n" (map deviceToKdl (cfg.settings.devices or []))}
  '';
in
{
  options.services.niri-mapper = {
    enable = lib.mkEnableOption "niri-mapper input remapping daemon";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.system}.niri-mapper;
      description = "The niri-mapper package to use";
    };

    settings = lib.mkOption {
      type = settingsType;
      default = {};
      description = "niri-mapper configuration (will be converted to KDL)";
      example = lib.literalExpression ''
        {
          global = {
            logLevel = "info";
            # Uses XDG config home by default
            # niriKeybindsPath = "~/.config/niri/niri-mapper-keybinds.kdl";
          };
          devices = [
            {
              name = "Keychron K3 Pro";
              profiles = {
                default = {
                  remap = {
                    CapsLock = "Escape";
                    Escape = "CapsLock";
                  };
                  niriPassthrough = [
                    { key = "Super+Return"; action = "spawn \"alacritty\";"; }
                    { key = "Super+d"; action = "spawn \"fuzzel\";"; }
                  ];
                  combo = {
                    "Ctrl+Shift+Q" = "Alt+F4";
                  };
                };
                gaming = {
                  remap = {};  # No remaps in gaming mode
                };
              };
              profileSwitch = {
                "Ctrl+Shift+1" = "default";
                "Ctrl+Shift+2" = "gaming";
              };
            }
          ];
        }
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    # Warning about input group requirement
    # Home Manager cannot configure system-level permissions (udev rules, kernel modules, group membership)
    # Users must ensure they are in the 'input' group to access /dev/input devices
    warnings = [
      ''
        niri-mapper (Home Manager): You must be a member of the 'input' group to use niri-mapper.

        Unlike the NixOS module, Home Manager cannot:
        - Add you to the 'input' group
        - Configure udev rules for /dev/uinput
        - Load the 'uinput' kernel module

        To fix this on NixOS, add to your system configuration:
          users.users.<your-username>.extraGroups = [ "input" ];
          boot.kernelModules = [ "uinput" ];
          services.udev.extraRules = '''
            KERNEL=="uinput", MODE="0660", GROUP="input", OPTIONS+="static_node=uinput"
          ''';

        On other Linux distributions, use your system's tools to:
        1. Add your user to the 'input' group: sudo usermod -aG input $USER
        2. Ensure the 'uinput' module is loaded: sudo modprobe uinput
        3. Configure udev rules for /dev/uinput access

        After making these changes, log out and back in for group membership to take effect.

        If the niri-mapper daemon fails to grab devices, this is likely the cause.
      ''
    ];

    # Configuration validation assertions
    assertions = [
      {
        assertion = cfg.settings.devices != [];
        message = "niri-mapper: At least one device must be configured when the service is enabled. Add devices to services.niri-mapper.settings.devices.";
      }
    ] ++ (lib.imap0 (idx: device: {
      assertion = device.profiles != {};
      message = "niri-mapper: Device at index ${toString idx} (\"${device.name}\") must have at least one profile configured.";
    }) cfg.settings.devices);

    home.packages = [ cfg.package ];

    xdg.configFile."niri-mapper/config.kdl".text = configToKdl cfg.settings;

    systemd.user.services.niri-mapper = {
      Unit = {
        Description = "niri-mapper input remapping daemon";
        After = [ "graphical-session.target" ];
        PartOf = [ "graphical-session.target" ];
      };

      Service = {
        Type = "simple";
        ExecStart = "${cfg.package}/bin/niri-mapperd --config ${config.xdg.configHome}/niri-mapper/config.kdl --foreground";
        ExecReload = "${pkgs.coreutils}/bin/kill -HUP $MAINPID";
        Restart = "on-failure";
        RestartSec = 5;
      };

      Install = {
        WantedBy = [ "graphical-session.target" ];
      };
    };
  };
}
