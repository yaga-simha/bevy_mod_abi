# `bevy_abi_mod_reflect`

Safe (`#![forbid(unsafe_code)]`) dynamic `bevy_reflect`, Bevy Remote Protocol (`bevy_remote` BRP), and `bevy_settings` TOML bridges for [`bevy_abi_mod`](../bevy_abi_mod).

## Overview

- **`ModReflectPlugin` / `ModReflectAppExt` / `ModReflectWorldExt`:** Synthesizes `DynamicStruct` / `ReflectFromPtr` / `ReflectComponent` / `ReflectResource` registrations from `ModComponent` witness schemas and declarative ABI `0.3` `BamFieldDesc` descriptors.
- **BRP Integration (`bevy_remote`):** Installs `bevy_abi_mod/*` JSON-RPC methods and enables standard Bevy BRP inspection/mutation for foreign C-ABI components and resources.
- **Settings Sync (`bevy-settings`):** Synchronizes mod configuration resources with persistent TOML files.
