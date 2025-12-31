# NixOS module for niri-mapper
self:
{ config, lib, pkgs, ... }:

let
  cfg = config.services.niri-mapper;

  # Type definitions for niri-mapper configuration

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
        default = "/etc/niri-mapper/keybinds.kdl";
        description = ''
          Path to write generated niri keybinds file.

          For system-level installation via NixOS module, this defaults to
          /etc/niri-mapper/keybinds.kdl. Users should include this file in
          their niri configuration.

          For user-level installation, consider using the home-manager module
          instead, which defaults to ~/.config/niri/niri-mapper-keybinds.kdl.
        '';
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
  configToKdl = config: let
    indent = level: lib.concatStrings (lib.genList (_: "    ") level);

    globalToKdl = global: ''
      global {
          log-level "${global.logLevel or "info"}"
          niri-keybinds-path "${global.niriKeybindsPath or "/etc/niri-mapper/keybinds.kdl"}"
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
      ${lib.optionalString (profile.remap or {} != {}) ''
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

    deviceToKdl = device: ''
      device "${device.name}" {
      ${lib.concatStringsSep "\n" (lib.mapAttrsToList (profileToKdl 1) (device.profiles or { default = {}; }))}
      }
    '';
  in ''
    // Generated by NixOS niri-mapper module
    ${globalToKdl (config.global or {})}

    ${lib.concatStringsSep "\n" (map deviceToKdl (config.devices or []))}
  '';

  configFile = pkgs.writeText "niri-mapper-config.kdl" (configToKdl cfg.settings);

  # Generate niri keybinds KDL (for declarative mode)
  # This mimics the Rust generator in crates/niri-mapper-config/src/generator.rs
  niriKeybindsToKdl = config: let
    # Translate Super -> Mod (niri convention)
    translateModifiers = key:
      lib.concatStringsSep "+" (map (part:
        if part == "Super" then "Mod" else part
      ) (lib.splitString "+" key));

    # Collect all niri-passthrough keybinds from all devices/profiles
    collectKeybinds = devices:
      lib.concatLists (map (device:
        lib.concatLists (lib.mapAttrsToList (_profileName: profile:
          map (keybind: {
            key = translateModifiers keybind.key;
            action = keybind.action;
          }) (profile.niriPassthrough or [])
        ) (device.profiles or {}))
      ) devices);

    keybinds = collectKeybinds (config.devices or []);

    keybindLines = lib.concatStringsSep "\n" (map (kb:
      "    ${kb.key} { ${kb.action} }"
    ) keybinds);
  in ''
    // Auto-generated by NixOS niri-mapper module (declarative mode)
    // DO NOT EDIT - changes will be overwritten on NixOS rebuild

    binds {
    ${keybindLines}
    }
  '';

  niriKeybindsFile = pkgs.writeText "niri-mapper-keybinds.kdl" (niriKeybindsToKdl cfg.settings);

  # Extract the relative path for environment.etc (strips leading /etc/)
  niriKeybindsEtcPath =
    let
      fullPath = cfg.settings.global.niriKeybindsPath;
    in
      if lib.hasPrefix "/etc/" fullPath
      then lib.removePrefix "/etc/" fullPath
      else null;
in
{
  options.services.niri-mapper = {
    enable = lib.mkEnableOption "niri-mapper input remapping daemon";

    declarativeNiriKeybinds = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Generate the niri keybinds file declaratively via Nix.

        When enabled, NixOS will generate the keybinds.kdl file from the
        niri-passthrough settings at system build time, placing it at the
        configured niriKeybindsPath.

        When disabled (default), the niri-mapper daemon generates the keybinds
        file at runtime. This allows for dynamic updates via SIGHUP reload.

        Enable this if you prefer the keybinds file to be managed by NixOS
        (rebuild to update) rather than generated dynamically by the daemon.

        Note: When enabled, the daemon will still attempt to generate the file
        at runtime, but it will be overwritten on each NixOS rebuild.
      '';
    };

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
        # To have Nix manage the keybinds file declaratively:
        # services.niri-mapper.declarativeNiriKeybinds = true;
        {
          global = {
            logLevel = "info";
            # Default: /etc/niri-mapper/keybinds.kdl
            # niriKeybindsPath = "/etc/niri-mapper/keybinds.kdl";
          };
          devices = [
            {
              name = "Keychron K3 Pro";
              profiles.default = {
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
            }
          ];
        }
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    # Configuration validation assertions
    assertions = [
      {
        assertion = cfg.settings.devices != [];
        message = "niri-mapper: At least one device must be configured when the service is enabled. Add devices to services.niri-mapper.settings.devices.";
      }
      {
        # declarativeNiriKeybinds requires path under /etc/ (environment.etc limitation)
        assertion = !cfg.declarativeNiriKeybinds || niriKeybindsEtcPath != null;
        message = "niri-mapper: declarativeNiriKeybinds requires niriKeybindsPath to be under /etc/. Current path: ${cfg.settings.global.niriKeybindsPath}";
      }
    ] ++ (lib.imap0 (idx: device: {
      assertion = device.profiles != {};
      message = "niri-mapper: Device at index ${toString idx} (\"${device.name}\") must have at least one profile configured.";
    }) cfg.settings.devices);

    environment.systemPackages = [ cfg.package ];

    systemd.services.niri-mapper = {
      description = "niri-mapper input remapping daemon";
      wantedBy = [ "graphical-session.target" ];
      after = [ "graphical-session.target" ];

      serviceConfig = {
        Type = "simple";
        ExecStart = "${cfg.package}/bin/niri-mapperd --config ${configFile} --foreground";
        ExecReload = "${pkgs.coreutils}/bin/kill -HUP $MAINPID";
        Restart = "on-failure";
        RestartSec = 5;

        # Security hardening - general restrictions
        PrivateTmp = true;
        ProtectSystem = "strict";
        ProtectHome = "read-only";
        # Allow writing to the niri keybinds output directory
        ReadWritePaths = [ (builtins.dirOf cfg.settings.global.niriKeybindsPath) ];
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectClock = true;
        ProtectHostname = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        RestrictNamespaces = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        RemoveIPC = true;
        PrivateUsers = false;  # Needs access to real user for device permissions

        # Must be false - we need to access /dev/input/* and /dev/uinput
        PrivateDevices = false;

        # Device access control - only allow input devices and uinput
        DevicePolicy = "closed";
        DeviceAllow = [
          "/dev/input/* rw"
          "/dev/uinput rw"
        ];

        # Capability restrictions - minimal set for input device access
        # CAP_DAC_READ_SEARCH: May be needed for reading device paths
        # Note: Most access is handled via input group membership, not capabilities
        CapabilityBoundingSet = [ "CAP_DAC_READ_SEARCH" ];
        AmbientCapabilities = [ ];

        # System call filtering - allow only needed syscalls
        SystemCallFilter = [ "@system-service" "~@privileged" "~@resources" ];
        SystemCallArchitectures = "native";
        SystemCallErrorNumber = "EPERM";

        # No new privileges (safe since we use group-based device access)
        NoNewPrivileges = true;

        # Required for input device access via group membership
        SupplementaryGroups = [ "input" ];
      };
    };

    # Ensure uinput module is loaded
    boot.kernelModules = [ "uinput" ];

    # udev rules for input device access
    services.udev.extraRules = ''
      # niri-mapper: allow access to uinput
      KERNEL=="uinput", MODE="0660", GROUP="input", OPTIONS+="static_node=uinput"
    '';

    # Create the niri keybinds output directory if using default system path
    # (only needed when NOT using declarative mode, since environment.etc handles it otherwise)
    systemd.tmpfiles.rules = lib.mkIf (lib.hasPrefix "/etc/" cfg.settings.global.niriKeybindsPath && !cfg.declarativeNiriKeybinds) [
      "d ${builtins.dirOf cfg.settings.global.niriKeybindsPath} 0755 root root -"
    ];

    # Generate niri keybinds file declaratively when enabled
    # This creates the file at the configured niriKeybindsPath under /etc/
    environment.etc = lib.mkIf (cfg.declarativeNiriKeybinds && niriKeybindsEtcPath != null) {
      ${niriKeybindsEtcPath} = {
        source = niriKeybindsFile;
        mode = "0644";
      };
    };
  };
}
