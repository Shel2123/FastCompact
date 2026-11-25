# Шаг 4.5: платформенные порты и `orbit-runtime`

Ядро (`orbit-domain`, `orbit-app`, `orbit-runtime`) не знает об ОС. Всё платформенное лежит за портами и за `Platform`; порт на другую ОС — новый crate с `impl Platform` и строка в `orbit-desktop`.

```text
orbit-domain → orbit-app → orbit-runtime → orbit-macos → orbit-desktop
```

## Где что

| Часть | Файл |
|---|---|
| Порты `MenuView`, `ParametersView`, `Feedback`, `Shell` и их контракт | `crates/orbit-app/src/ports.rs` |
| `Platform`, `Views`, `ResolverFactory` | `crates/orbit-runtime/src/platform.rs` |
| `Runtime`, `run`, `RunError` | `crates/orbit-runtime/src/runtime.rs` |
| Эффект → вызов порта (чистая диспетчеризация) | `crates/orbit-runtime/src/effects.rs` |
| Resolver-поток, пул задач | `crates/orbit-runtime/src/{resolver,jobs}.rs` |
| `MacPlatform`, порядок запуска | `crates/orbit-macos/src/mac_platform.rs` |
| Опции и отладочные переключатели | `crates/orbit-macos/src/debug.rs` (`MacOptions`) |
| Логирующие заглушки видов и shell (меню заменено `RadialMenu` на этапе 5, остальное до этапов 6–8) | `crates/orbit-macos/src/ui/logging.rs` |
| Заглушка операций (до этапа 7) | `crates/orbit-macos/src/operations/unavailable.rs` |
| Правило имён файлов | `FileNamePolicy` в `crates/orbit-domain/src/operation.rs` |
| Проверка переносимости | `scripts/check-portable.sh` |

## Порядок запуска macOS

`MacPlatform::new` (tap, sender) → `orbit_runtime::run` (resolver-поток, пул, `Runtime`) → `Platform::run`: handler → хоткей → `NSApplication::run`. События tap до установки handler ждут в sink и приходят первыми, по порядку.

Отличие от этапа 4: значок в строке меню и политика активации создаются до `set_handler`, а не после (на порядок событий не влияет).

## Эффекты и порты

| Эффект | Вызовы |
|---|---|
| `PresentMenu`, `Highlight`, `HideMenu` | `MenuView::{present, highlight, hide}` |
| `PresentParameters`, `ParametersInvalid`, `HideParameters` | `ParametersView::{present, show_invalid, hide}` |
| `JobProgress`, `Notify`, `ShowInfo` | `Feedback::{job_progress, notify, show_info}` |
| `WriteClipboard`, `RevealFile` | `Shell::{write_clipboard, reveal}`; при `Err` затем `Feedback::notify(Notice::ActionFailed)` |
| `ResolveTarget`, `StartJob`, `CancelJob` | портов нет: resolver-поток и пул задач, ответы событиями через sink |

## Проверки

- `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-features`.
- `scripts/check-portable.sh`: три ядерных crate собираются для `x86_64-pc-windows-msvc` (с тестами и clippy), в их дереве зависимостей нет macOS-пакетов, в исходниках нет `objc2`, `dispatch2`, `target_os`. Это проверка компиляции, не поведения: на Windows тесты не запускаются.
- Ручной прогон после переезда: `scripts/bundle.sh`, затем S1, S2, S4, S5, S6, S8, S12 из `docs/stage-4.md`. Результат — в таблице ниже.

| № | Результат |
|---|---|
| S1 | |
| S2 | |
| S4 | |
| S5 | |
| S6 | |
| S8 | |
| S12 | |

## Долг

- Путь лога `~/Library/Logs/Orbit/orbit.log` и путь блокировки второго экземпляра лежат в `orbit-desktop` и привязаны к macOS. Вынести в `Platform` при порте.
- S5b, S12c, S14 из `docs/stage-4.md` не прогнаны.
- `Runtime` не дропается при выходе из `NSApplication::run`: процесс завершается, потоки не останавливаются отдельно.
