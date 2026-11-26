# winmidi2 后端 hotplug 适配说明

本文记录 `winmidi2` 后端对 midir hotplug API 的适配，包括改动内容、运行流程、bindings 生成方式和验证方法。

## 1. 背景

- `9e5d188` 在 `src/common.rs` 中加入公开 API：`PortEvent` 和 `PortWatcher<T>`。
- `99cdf4f` 为 ALSA 后端实现了 `PortWatcher`。
- `4996f68` 加入 `winmidi2` 后端（`Windows.Devices.Midi2`），其中已经有一个进程级的 `MidiLegacyPortDeviceWatcher` 用于维护端口列表，但当时没有实现 hotplug。
- 本次改动为 `winmidi2` 实现 `PortWatcher`，复用现有的端口 watcher，避免额外枚举并保证回调中看到的端口列表状态一致。

## 2. 改动文件

| 文件 | 改动 |
| --- | --- |
| `src/backend/winmidi2/hotplug.rs` | 新增 `PortWatcher<T>` 实现，负责注册 winmidi2 watcher 事件、转换 `PortEvent`、管理回调和用户数据。 |
| `src/backend/winmidi2/watcher.rs` | 将 `current()` 改为 `pub(super)`，供 hotplug 获取共享的 `MidiLegacyPortDeviceWatcher`。 |
| `src/backend/winmidi2/mod.rs` | 注册 `mod hotplug` 并导出 `PortWatcher`；为 `MidiInputPort` / `MidiOutputPort` 增加 `Debug` 派生（`PortEvent` 需要）。 |
| `src/backend/winmidi2/bindings.rs` | 从 `Windows.Devices.Midi2.0.99.92-preview.11.nupkg` 的 winmd 重新生成，加入 Added/Removed 事件参数类型和 watcher 的 `Added` / `Removed` 方法。 |
| `src/backend/winmidi2/tests.rs` | 新增 loopback 端口 hotplug 测试，验证 input/output 的添加和移除事件。 |
| `tools/winmidi2-bindgen/src/main.rs` | 在 `TYPES` 中加入 `MidiLegacyPortDeviceInformationAddedEventArgs` 和 `RemovedEventArgs`，使以后重新生成 bindings 时能生成事件方法。 |
| `README.md` | 更新 Windows MIDI Services 的端口变化说明，介绍 `PortWatcher` 及其限制。 |
| `examples/test_port_watcher_virtual.rs` | 将仅 Unix 使用的导入加上 `#[cfg(unix)]`，避免 Windows 构建出现未使用导入警告。 |

## 3. 设计

### 3.1 复用共享 watcher

`winmidi2` 后端已经有一个进程级的 `MidiLegacyPortDeviceWatcher`（定义在 `src/backend/winmidi2/watcher.rs` 的 `STATE` 中），用于给 `MidiInput::ports()` / `MidiOutput::ports()` 提供缓存列表。

`PortWatcher::new` 调用 `watcher::current()` 获取这个共享 watcher：

- 如果 watcher 尚未启动，`current()` 会启动它并等待首次 `EnumerationCompleted`，最多 10 秒；失败则返回 `None`，`PortWatcher::new` 返回 `InitError`。
- 如果 watcher 已经在运行，直接复用。

这样做有三个好处：

1. 不需要为每个 `PortWatcher` 再做一次 Plug and Play 枚举。
2. Windows MIDI Services 在 watcher 内部先更新端口映射，再抛出 `Added` / `Removed` 事件。因此回调执行时，`ports()`、`port_count()`、`port_name()` 已经能看到新状态。
3. 多个 `PortWatcher` 和端口列表共用同一个 watcher，状态一致。

### 3.2 首次枚举不产生事件

`watcher::current()` 返回时首次枚举已经完成。`PortWatcher::new` 在这之后才注册 `Added` / `Removed` 处理器，因此：

- 创建 watcher 时已经存在的端口不会被报告为 `InputAdded` / `OutputAdded`。
- 只有 watcher 创建之后发生的端口添加和移除才会通过回调报告。

这与 `PortWatcher` 的 API 语义一致。

### 3.3 事件转换

`PortWatcher` 监听 `MidiLegacyPortDeviceWatcher` 的：

- `Added`：参数类型 `MidiLegacyPortDeviceInformationAddedEventArgs`。
- `Removed`：参数类型 `MidiLegacyPortDeviceInformationRemovedEventArgs`。

转换规则：

| watcher 事件 | `Midi1PortFlow` | midir 事件 |
| --- | --- | --- |
| `Added` | `MidiMessageSource` | `PortEvent::InputAdded(MidiInputPort)` |
| `Removed` | `MidiMessageSource` | `PortEvent::InputRemoved(MidiInputPort)` |
| `Added` | `MidiMessageDestination` | `PortEvent::OutputAdded(MidiOutputPort)` |
| `Removed` | `MidiMessageDestination` | `PortEvent::OutputRemoved(MidiOutputPort)` |
| 其它值 | — | 忽略 |

端口字段来自 `super::port_info()`：

- `PortDeviceId` → `port_device_id`
- `AssociatedEndpointDeviceId` → `endpoint_device_id`
- `Group` → `group`

缺少 Group、ID 为空或 Group 超出范围的端口会被忽略，和 `ports()` 的过滤规则一致。

### 3.4 回调、线程与 panic

- WinRT watcher 事件在 Windows 设备枚举线程或线程池线程上触发，不在应用程序创建 `PortWatcher` 的线程上。
- 每个 `PortWatcher` 的 `CallbackData<T>` 由 `Mutex` 保护，因此同一个 `PortWatcher` 的回调是串行执行的，用户数据不需要额外加锁。
- 用户回调包裹在 `catch_unwind` 中，panic 不允许穿过 WinRT FFI 边界；发生 panic 时记录错误并丢弃该事件。
- `T` 必须满足 `Send`，与公开 API 的约束一致。

### 3.5 停止与清理

`PortWatcher::stop()` 和 `Drop` 共用 `shutdown()`：

1. 获取 `Mutex`，把 `active` 置为 `false`，并取走用户数据。
2. 等待当前正在执行的回调结束（通过互斥锁自然同步）。
3. 对共享 watcher 调用 `RemoveAdded` / `RemoveRemoved`，只注销当前 `PortWatcher` 的 token。
4. 不会调用共享 watcher 的 `Stop()`，因为端口列表和其它 `PortWatcher` 仍在使用它。

注销 token 之后才到达的回调会看到 `active == false`，直接返回，不再访问用户数据。

### 3.6 已知限制

- **端口重命名不会产生事件。** `MidiLegacyPortDeviceWatcher` 的 `Updated` 事件表示名称或 WinMM 端口号变化，但 `PortEvent` 只有 Added/Removed。`PortWatcher` 不订阅 `Updated`；共享列表会更新名称，之后调用 `port_name()` 能拿到新名称，但不会有事件通知。
- **Windows MIDI 服务重启时不会自动迁移。** 如果 watcher 因服务重启等原因停止，已有 `PortWatcher` 绑定在旧的 watcher 上；重新创建 `PortWatcher` 即可恢复通知。
- **其它后端未实现。** 本分支上 `winmm`、`winrt` 等后端仍未实现 `PortWatcher`，不属于本次 winmidi2 适配范围。

## 4. 运行流程

### 4.1 创建 `PortWatcher`

```
应用
  └─ PortWatcher::new(client_name, callback, data)
       ├─ ensure_available()
       │    ├─ 检查 Windows 版本 >= 11 25H2
       │    ├─ MidiApi::EnsureServiceAvailable()
       │    └─ 检查 API 是否由系统提供或随应用分发
       ├─ watcher::current()
       │    ├─ 尚未启动：Create() + 注册内部事件 + Start()
       │    ├─ 等待 EnumerationCompleted（最多 10 秒）
       │    └─ 返回共享 MidiLegacyPortDeviceWatcher
       ├─ 在共享 watcher 上注册 Added 处理器
       ├─ 在共享 watcher 上注册 Removed 处理器
       └─ 返回 PortWatcher<T>
```

### 4.2 端口添加

```
Windows MIDI Services / DeviceWatcher
  └─ 端口出现
       ├─ MidiLegacyPortDeviceWatcher 内部更新端口映射
       ├─ 抛出 Added(AddedEventArgs)
       └─ midir 的 Added 处理器
            ├─ AddedDevice() 取得 MidiLegacyPortDeviceInformation
            ├─ 按 Flow 转换为 InputAdded 或 OutputAdded
            ├─ 加锁检查 active
            └─ 调用用户 callback(event, &mut data)
```

回调执行期间共享 watcher 的列表已经包含新端口，因此回调里可以立即调用 `MidiInput::ports()` / `MidiOutput::ports()`。

### 4.3 端口移除

```
Windows MIDI Services / DeviceWatcher
  └─ 端口消失
       ├─ MidiLegacyPortDeviceWatcher 内部移除端口
       ├─ 抛出 Removed(RemovedEventArgs)
       └─ midir 的 Removed 处理器
            ├─ RemovedDevice() 取得端口信息
            ├─ 按 Flow 转换为 InputRemoved / OutputRemoved
            ├─ 加锁检查 active
            └─ 调用用户 callback(event, &mut data)
```

### 4.4 停止

```
PortWatcher::stop() 或 Drop
  └─ shutdown()
       ├─ active = false，取走用户数据
       ├─ RemoveAdded(token)
       ├─ RemoveRemoved(token)
       └─ 返回用户数据（仅 stop()）
```

## 5. bindings 重新生成

`src/backend/winmidi2/bindings.rs` 是生成文件，不应手工修改。生成流程：

1. 从 NuGet 包取得 winmd。本次使用的包：

   ```
   /home/headcrab/Windows.Devices.Midi2.0.99.92-preview.11.nupkg
   ```

   解压出：

   ```
   ref/native/Windows.Devices.Midi2.winmd
   ```

2. 运行生成工具：

   ```bash
   cargo run --manifest-path tools/winmidi2-bindgen/Cargo.toml -- \
       /path/to/Windows.Devices.Midi2.winmd
   ```

   工具会写入 `src/backend/winmidi2/bindings.rs`，执行针对 `[NoException]` 静态方法的修正，并调用 `rustfmt`。

3. `TYPES` 中必须列出事件参数类型：

   ```rust
   "Windows.Devices.Midi2.Enumeration.Legacy.MidiLegacyPortDeviceInformationAddedEventArgs",
   "Windows.Devices.Midi2.Enumeration.Legacy.MidiLegacyPortDeviceInformationRemovedEventArgs",
   ```

   `windows-bindgen` 只会为依赖类型都在筛选列表中的事件生成 `Added` / `Removed` 方法；缺少参数类型时，vtable 中对应槽位会退化为 `usize`，无法注册处理器。

## 6. 验证

### 6.1 编译检查

在 Linux 上交叉检查 Windows target：

```bash
cargo check --all-targets --target x86_64-pc-windows-msvc --features winmidi2
cargo clippy --all-targets --target x86_64-pc-windows-msvc --features winmidi2
```

本次改动已在上述命令下通过。输出中的 `MidiMessage` dead code 和 `parking_lot` unused dependency 警告是 winmidi2 后端已有的，与本次 hotplug 改动无关。

### 6.2 格式检查

```bash
rustfmt --edition 2021 --check \
    src/backend/winmidi2/hotplug.rs \
    src/backend/winmidi2/mod.rs \
    src/backend/winmidi2/tests.rs \
    src/backend/winmidi2/watcher.rs \
    src/backend/winmidi2/bindings.rs
```

### 6.3 运行测试

需要有 Windows 11 25H2、Windows MIDI Services 服务以及对应的 `Windows.Devices.Midi2.dll`：

```bash
cargo test --features winmidi2
```

新增测试 `port_watcher_reports_loopback_ports` 会：

1. 创建 `PortWatcher`。
2. 创建临时 loopback endpoint pair。
3. 等待 loopback 端口的 `InputAdded` / `OutputAdded` 事件。
4. 删除 loopback。
5. 等待对应端口的 `InputRemoved` / `OutputRemoved` 事件。

在本次开发所用的 Linux 环境中，由于缺少 ALSA 开发包（`alsa.pc`），无法运行 Linux 侧的 `cargo test`；Windows 测试已通过 `--tests` 类型检查，但实际运行需要 Windows MIDI Services 环境。

## 7. 使用示例

```rust
use midir::{PortEvent, PortWatcher};

let watcher = PortWatcher::new(
    "my-app",
    |event, changed: &mut bool| {
        *changed = true;
        match event {
            PortEvent::InputAdded(port) => println!("input added: {}", port.id()),
            PortEvent::InputRemoved(port) => println!("input removed: {}", port.id()),
            PortEvent::OutputAdded(port) => println!("output added: {}", port.id()),
            PortEvent::OutputRemoved(port) => println!("output removed: {}", port.id()),
        }
    },
    false,
)
.expect("create port watcher");

// 在事件回调之后，重新读取端口列表即可拿到最新状态。
// ...

let changed = watcher.stop();
println!("ports changed: {changed}");
```
