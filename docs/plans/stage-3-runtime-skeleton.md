# План этапа 3: runtime-скелет и порты

## Context
Этап 2 закоммичен (`08d2ce9`): чистый `AppController::update(event) -> Vec<Effect>`. Этап 3 — то, что исполняет эффекты и доставляет события обратно на main:
- порты в `orbit-app`;
- поток resolver'а и пул workers в `orbit-desktop`;
- `MainThreadSender` в `orbit-macos`;
- бинарник `orbit` как accessory-приложение AppKit.

Хоткея и настоящего резолва ещё нет (этап 4), операций тоже (этап 7). Порты закрыты заглушками. Цель: контракты доставки (FIFO, без повторного входа, завершение задачи доставляется всегда) закрыть тестами до того, как к ним подключится платформа.

Решения пользователя (2026-10-26):
- Бинарник: AppKit, иконка в строке меню с пунктом Quit, настоящая main-очередь, порты-заглушки.
- Resolver: из очереди выполняется только последний запрос, более старые отбрасываются без ответа.

## Зависимости
- workspace: `crossbeam-channel = "0.5.17"` (уже в `Cargo.lock` через global-hotkey).
- `orbit-macos` `[dependencies]`: `dispatch2`, `objc2`, `objc2-app-kit` (NSApplication, NSStatusBar, NSStatusItem, NSStatusBarButton, NSMenu, NSMenuItem, NSButton, NSControl, NSResponder, NSView), `objc2-foundation` (NSString), `tracing`. Spike остаётся на dev-dependencies, Cargo объединяет features.
- `orbit-macos` dev: `objc2-core-foundation` CFRunLoop — уже есть, нужен для теста main-очереди.
- `orbit-desktop`: `orbit-domain`, `crossbeam-channel`, `objc2`, `objc2-app-kit` (NSApplication), `tracing`, `tracing-subscriber`.

## `orbit-app/src/ports.rs` — порты
```rust
/// Доставка событий контроллеру. Контракт ниже.
pub trait EventSink: Send + Sync { fn send(&self, event: AppEvent); }

pub struct ResolveRequest { pub gesture: GestureId, pub pressed_at: Instant }
/// Не `Send`: реализация держит AX-хэндлы. Создаётся, работает и
/// уничтожается в одном потоке resolver'а.
pub trait TargetResolver {
    fn resolve(&mut self, request: &ResolveRequest) -> Result<FileTarget, ResolveError>;
}

pub trait OperationExecutor: Send + Sync {
    fn execute(&self, spec: &JobSpec, context: &JobContext) -> Result<JobOutput, JobError>;
}
pub struct CancelToken(Arc<AtomicBool>);              // Clone; new / cancel / is_cancelled
pub struct JobContext { cancel: CancelToken, progress: Box<dyn Fn(Progress) + Send + Sync> }
// JobContext::new(cancel, progress), is_cancelled(), report(Progress)
```
- **Контракт `EventSink`** (записывается в doc):
  1. **Порядок по happens-before, из любых потоков.** Если `send(A)` вернулся до начала `send(B)`, A доставляется раньше B. Это верно, даже если A и B отправлены из разных потоков: например, прогресс из вспомогательного потока исполнителя и завершение из worker'а. Одновременные `send` из разных потоков порядка не имеют; там контроллер сверяется по id.
  2. `send` никогда не вызывает обработчик синхронно, поэтому повторного входа в контроллер нет.
  3. `send` не блокирует и ничего не теряет, пока приложение работает. После выхода события отбрасываются.
- `TargetResolver` без `Send` и `Sync`: настоящий resolver этапа 4 держит `!Send` AX-хэндлы (этап 0). В поток передаётся `Send`-фабрика, а сам resolver создаётся и уничтожается внутри потока.
- `JobContext` передаётся по ссылке `&JobContext`, не `Clone`. Исполнитель может звать `report` из scoped-потоков: они завершаются до возврата `execute`, поэтому по пункту 1 весь прогресс доставляется раньше `JobFinished`. Поток `'static` ссылку на контекст получить не может.
- Реестра операций (OperationKind → executor) пока нет. Он появится на этапе 7 как реализация `OperationExecutor`.
- **Изменение публичного API:** `JobError::Internal(String)` — паника исполнителя, остановленные workers, заглушка. Контроллер по видам ошибок не ветвится, поэтому код этапа 2 не меняется.

## `orbit-macos/src/main_thread.rs` — доставка на main
- **`Mailbox<E>`** — чистая логика, без AppKit, тестируется unit-тестами:
  - `queue: RefCell<VecDeque<E>>` и `handler: RefCell<Option<Box<dyn FnMut(E)>>>` — два раздельных `RefCell`;
  - `deliver(e)`: положить в очередь (короткий borrow). Если `handler` уже занят (вложенный run loop, например модальное окно внутри обработчика), выйти: внешний цикл заберёт событие. Иначе — цикл `pop → handler(e)`, пока очередь не опустеет;
  - `set_handler(h)`: поставить обработчик и сразу разобрать очередь. События, пришедшие до установки, не теряются.
- **`MainThreadSender`**: `Clone + Send + Sync`, реализует `EventSink`.
  - `new(mtm)`: создаёт `Mailbox<AppEvent>` в `thread_local` main. Повторный вызов → ошибка.
  - `set_handler(mtm, handler)`: отдельный шаг, потому что runtime сам нуждается в sender'е при создании.
  - `send(e)` = `DispatchQueue::main().exec_async(move || MAILBOX.with(|m| m.deliver(e)))`.
    - Пункт 1: `dispatch_async` ставит блок в очередь до возврата, а main-очередь серийная FIFO. Значит, если `send(A)` вернулся раньше начала `send(B)`, блок A стоит перед блоком B, из какого бы потока ни пришли вызовы. `Mailbox` порядок тоже сохраняет.
    - Пункт 2: `exec_async` откладывает вызов обработчика.

## `orbit-macos/src/ui/status_item.rs` — значок в строке меню
- Новый модуль, в раскладке его не было. `status_item(mtm) -> Retained<NSStatusItem>`: «◎» и меню с «Quit Orbit» (`terminate:`, ⌘Q). Код взят из spike.

## `orbit-desktop/src/runtime/` — исполнение
### `resolver.rs` — `ResolverThread`
- `spawn<F, R>(make: F, sink: Arc<dyn EventSink>) -> io::Result<Self>`, где `F: FnOnce() -> R + Send + 'static` и `R: TargetResolver + 'static`.
  - Resolver создаётся первым действием потока и уничтожается при выходе из него. Через границу потока переходит только фабрика.
  - Фабрика пока не может вернуть ошибку: для AX-resolver'а это системный элемент. Если этапу 4 понадобится ошибка, тип поменяется там.
- Канал unbounded: запросы приходят с человеческой частотой и весят мало.
- Поля `sender: Option<Sender<ResolveRequest>>`, `thread: Option<JoinHandle<()>>`.
- Цикл потока:
  - `recv()`, затем `try_recv()`, пока очередь не опустеет; остаётся последний запрос, отброшенные пишутся в `debug`;
  - `resolve`, ответ `TargetResolved { gesture, result }`.
- Почему пропуск безопасен: новый `ResolveTarget` контроллер выдаёт только для нового жеста, после чего состояние уже `Resolving` с новым id. Значит, отброшенный запрос никогда не текущий и ответа не ждёт.
- Паника в `resolve` ловится через `catch_unwind(AssertUnwindSafe)` и отвечает `Err(Failed("resolver panicked"))`; поток продолжает работу.
  - Требование к реализациям, записывается в doc трейта: состояние между вызовами — только кэш, после паники resolver остаётся пригодным.
- `request(&self, req) -> Result<(), ResolverStopped>`. Ошибка, если поток завершился или после `shutdown`, когда `sender == None`.
- `shutdown(&mut self)`: забрать `sender` и закрыть канал, затем забрать `thread` и дождаться его. Повторный вызов ничего не делает. При выходе из приложения `Drop` не ждёт поток (AX-вызов может висеть).

### `jobs.rs` — `JobRunner`
- `new(executor: Arc<dyn OperationExecutor>, sink, JobsConfig { workers: 2, queue: 8 }) -> io::Result<Self>`. Канал bounded crossbeam; `tokens: HashMap<JobId, CancelToken>` хранит main.
- `start(spec)`:
  - завести токен;
  - `try_send(Work { spec, token })`;
  - `Full` → убрать токен и отправить `JobFinished(Err(QueueFull))` через sink;
  - `Disconnected` → `JobFinished(Err(Internal("workers stopped")))`.
  - Ответ всегда идёт через sink, не напрямую в контроллер: так нет повторного входа. Задача к этому моменту уже записана в `jobs` контроллера.
- `cancel(id)` → `token.cancel()`. Неизвестный id → `debug`.
- `finished(id)` убирает токен; вызывается runtime на `JobFinished` перед контроллером.
- Worker, для каждой задачи:
  1. Токен уже отменён → `Err(Cancelled)`, `execute` не вызывается.
  2. Иначе `catch_unwind(execute)`; паника → `Err(Internal("operation panicked"))`.
  3. **`JobFinished` отправляется всегда**, даже `Ok` после отмены: гонку разбирает контроллер.
- Прогресс: `JobContext::report` троттлится.
  - Решение принимает чистый `Throttle { interval, last: Option<Instant> }` с методом `admit(&mut self, now: Instant) -> bool`: пропускает, если `last == None` или `now - last >= interval`, и запоминает `now`. Clock-трейта нет: в тестах `now` подаётся явно.
  - Worker держит `Mutex<Throttle>` (интервал 100 мс) и зовёт `admit(Instant::now())`. Пропущенное значение уходит в `sink.send` прямо внутри `report`, до его возврата.
  - Промежуточные значения теряются (коалесцирование), завершение — никогда.
  - Прогресс раньше завершения: `report` отправляет событие до возврата, все вызовы `report` (из любых scoped-потоков) завершаются до возврата `execute`, а `JobFinished` worker отправляет после. По пункту 1 контракта `EventSink` доставка идёт в этом порядке.
- `shutdown(&mut self)`: отменить все токены, закрыть канал (`Option<Sender>`), дождаться workers (`Vec<JoinHandle>` через `drain`). Задачи из очереди быстро завершатся с `Cancelled`. После `shutdown` `start` даёт `Disconnected` → `JobFinished(Err(Internal("workers stopped")))`.

### `effects.rs` — исполнение эффектов
- `execute(effect, &mut Services)`, полный `match` без `_`:
  - `ResolveTarget` → `resolver.request`. `ResolverStopped` → `sink.send(TargetResolved(Err(Failed("resolver stopped"))))`: отказ, меню не откроется;
  - `StartJob` → `jobs.start`, `CancelJob` → `jobs.cancel`;
  - UI-эффекты и мгновенные действия (`PresentMenu`, `Highlight`, `HideMenu`, `PresentParameters`, `ParametersInvalid`, `HideParameters`, `WriteClipboard`, `RevealFile`, `ShowInfo`, `JobProgress`, `Notify`) → `info!` с содержимым. Их заменят этапы 5, 6 и 8.

### `mod.rs` — `Runtime`
- Поля: `controller`, `resolver`, `jobs`, `sink`.
- `new(sink, make_resolver: F, Arc<dyn OperationExecutor>, JobsConfig) -> io::Result<Self>`; `F` — та же `Send`-фабрика, что у `ResolverThread::spawn`.
- `handle(&mut self, event)`:
  1. `JobFinished { job }` → сначала `jobs.finished(job)`;
  2. `controller.update(event)`;
  3. эффекты исполняются по порядку.

## `orbit-desktop/src/main.rs` и `stubs.rs`
- Логи: stderr + `~/Library/Logs/Orbit/orbit.log`, тот же приём, что в spike. Без файла — только stderr и `warn`.
- `MainThreadMarker`, `NSApplication` accessory, `status_item`, `MainThreadSender::new`, `Runtime::new(...)`, `set_handler(move |e| runtime.handle(e))`, `app.run()`.
- `stubs.rs`:
  - `UnavailableResolver` → `Err(Failed("target resolution is not implemented yet"))`;
  - `UnavailableExecutor` → `Err(Internal("operations are not implemented yet"))`.
  - Удаляются на этапах 4 и 7.
- Ошибка при старте (поток не создан, повторный `new`) → `error!` и выход без `app.run()`.
- Защита от второго экземпляра — на этапе 4, вместе с регистрацией хоткея.

## Тесты
- **`Mailbox`** (unit, orbit-macos):
  - порядок сохраняется;
  - `deliver` изнутри обработчика (модель вложенного run loop) ставит событие в очередь без повторного входа и обрабатывает его после текущего, порядок `1, 2, 3`;
  - события до `set_handler` буферизуются и разбираются при установке.
- **`tests/main_thread.rs`** (`harness = false`, main-поток процесса):
  - `MainThreadSender` + обработчик;
  - 3 потока по 200 событий; порядок внутри потока сохранён, все доставлены на main;
  - межпоточный happens-before: поток 1 шлёт A и по каналу будит поток 2, тот шлёт B → доставлено A, затем B (100 повторов);
  - разбор очереди через `CFRunLoopRunInMode`, таймаут 5 с; не уложились → тест падает.
- **resolver** (desktop, фейк + `ChannelSink` на crossbeam с `recv_timeout`):
  - фейк держит в состоянии `Rc<()>`, то есть он `!Send`. Тест компилируется и работает только потому, что resolver создаётся фабрикой внутри потока;
  - ответ несёт id запроса;
  - последний побеждает: фейк заблокирован на A, пришли B и C → ответы только A и C;
  - после паники ответ `Failed`, следующий запрос снова обслужен;
  - после `shutdown(&mut self)` `request` возвращает `Err(ResolverStopped)`; повторный `shutdown` ничего не делает.
- **jobs:**
  - `Ok` → `JobFinished(Ok)`;
  - workers 1, queue 1, исполнитель заблокирован: третья задача сразу `QueueFull`;
  - отмена до старта → `Cancelled`, `execute` не вызван;
  - отмена во время работы видна через `is_cancelled`;
  - паника → `Internal`, worker жив и берёт следующую задачу;
  - `Throttle` с интервалом 100 мс и явным временем: вызовы на 0, 99, 100, 199 и 200 мс от базового `Instant` пропускают ровно 0, 100 и 200; первое значение проходит сразу;
  - прогресс из вспомогательного потока: исполнитель в `thread::scope` шлёт `report` из scoped-потока (с паузой ≥ 100 мс между значениями, чтобы троттлинг их пропустил), затем возвращает `Ok`. В sink все `JobProgress` идут раньше `JobFinished`;
  - `shutdown` отменяет задачи в очереди; `start` после `shutdown` → `JobFinished(Err(Internal("workers stopped")))`.
- **Runtime** (сквозной путь через фейки):
  - `Invoked` → `TargetResolved` из sink → состояние `Open`;
  - отпускание над ZIP: `slot_point`, как в тестах контроллера → `JobFinished` из sink → `jobs` контроллера пуст, токен убран;
  - resolver остановлен → состояние `Closed`, отказ;
  - `QueueFull` → задача удалена из состояния.

## Файлы
- `Cargo.toml` (workspace).
- `crates/orbit-app/src/{ports, messages}.rs`.
- `crates/orbit-macos/{Cargo.toml, src/main_thread.rs, src/ui/{mod, status_item}.rs, tests/main_thread.rs}`.
- `crates/orbit-desktop/{Cargo.toml, src/main.rs, src/stubs.rs, src/runtime/{mod, resolver, jobs, effects}.rs}`.
- Память после реализации: `orbit-event-effect-flow` (фактические порты и контракт `EventSink`), `orbit-threading-state` (resolver «последний побеждает», jobs), `orbit-crate-layout` (status_item, stubs), `orbit-implementation-plan` (статус).

## Verification
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo test --workspace --all-features` — app, domain, macos (Mailbox + main_thread) и desktop зелёные, spike собирается.
- Вручную: `cargo run -p orbit-desktop`.
  - В строке меню появляется «◎», в логе — `runtime started`.
  - Quit (⌘Q из меню) завершает процесс.
- Коммит — только по просьбе.
