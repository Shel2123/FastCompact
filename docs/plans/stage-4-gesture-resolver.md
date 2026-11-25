# План этапа 4: жест с tap → `FinderSelectionResolver` (без UI)

## Context

Этапы 0–3 готовы (`189d3b6`). Цель этапа 4: настоящий жест без визуальной части. В него входят:
- нажатие и выдача `GestureId`;
- чтение выделения Finder (I2-default) с guard ввода;
- отпускание, Esc и движение;
- защита от второго экземпляра.

Меню, подсветка и уведомления пока остаются `info!` в `effects.rs` (этап 5), но жест проходит целиком до эффекта в логе.

Ревизия 1 (после замечаний P1 к первой версии):
1. **Единственный источник событий жеста — tap.** У Carbon и tap две очереди без общей метки, и корреляция по времени ненадёжна. Carbon только поглощает комбинацию, его события жест не создают.
2. **Забор (fence).** Пауза после чтения не доказывает, что tap обработал все события до конца чтения. Поэтому resolver после чтения вводит маркер в поток событий и ждёт, пока tap его увидит; нет подтверждения — отказ. Guard сравнивает порядковые номера событий одного потока tap, а не `Instant`.
3. **Потерянное отпускание → `GestureCancelled(old)`, `Released(old)`, `Invoked(new)`**, а не синтетический `Released`.

Ревизия 2 (после замечаний ко второй версии):
4. **Ранний Esc.** Tap активный (`kCGEventTapOptionDefault`), и Esc удерживаемого жеста поглощается синхронно в callback по состоянию трекера. Регистрации Esc в Carbon нет совсем.
5. **Secure Input при нажатии.** В этом режиме tap не получает клавиатуру, и `Invoked` не возникает. Поэтому Carbon `Pressed` сам отказывает с `SecureInputOn`. Жест по-прежнему создаёт только tap.
6. **Secure Input во время удержания.** Пока жест удержан, в потоке tap работает сторожевой таймер. Потеря наблюдения → `Cancel(held)`, `Release(held)` и `record_interrupt`.
7. **Drag в обход guard.** Нажатие при зажатой кнопке мыши → `ButtonHeld` → отказ. `*Dragged` и mouse-up считаются внешним вводом.

Ревизия 3 (после замечаний к ревизии 2):
8. **Пропуск отпускания между опросами сторожа.** Опрос, даже частый, не даёт гарантии: Secure Input может включиться и выключиться между двумя опросами. Поэтому `Release` больше не выдаётся сразу. Сначала он проходит **проверку непрерывности**:
   - HID считает все клавиатурные события (`CGEventSourceCounterForEventType(HIDSystemState, KeyDown/KeyUp)`).
   - Сумма этих счётчиков между нажатием и отпусканием должна совпасть с числом клавиатурных событий, которые увидел tap.
   - Несовпадение или отсутствие подтверждения → `Cancel` + `Release`.
   - Сторож остаётся быстрым путём для длинного Secure Input.
   - Механизм проверяет spike F7b. Без подтверждения от F7b код этапа не начинаем.
9. **Готовность tap.** Созданный tap ещё не значит, что клавиатура до него доходит: Apple разрешает создать tap с урезанной маской.
   - Готовность включает фактическую маску (`CGGetEventTapList` → `eventsOfInterest`): в ней должна быть вся требуемая маска, tap должен быть `enabled`, preflight-проверки разрешений должны проходить.
   - Плюс инвариант на каждое нажатие: Carbon `Pressed`, для которого tap не увидел key-down хоткея, всегда даёт `InvokeRefused`.
   - Матрица разрешений проверяется в spike (шаг 0c).
10. **Жизненный цикл забора.**
    - Nonce регистрируется до постинга маркера.
    - Снимок хранится по nonce до `wait`.
    - Маркеры Orbit всегда поглощаются, включая поздние.
11. **Координаты.** Пересчёт Quartz → Cocoa идёт через основной экран (`NSScreen.screens[0]`, origin (0,0), полный `frame`), а не через `NSScreen.main` (это экран с фокусом клавиатуры). `visibleFrame` используется только для пределов размещения.

## Шаг 0. Проверки платформы — ✅ (`c8c9d5e`, `docs/spike-0.md`)

- F1 ✅ Tap видит key-down хоткея (с флагом autorepeat), key-up Space, FlagsChanged, Esc. Carbon `Pressed` приходит раз на нажатие, Carbon `Released` — только на key-up Space.
- F2 ✅ Маркер — `ScrollWheel` с нулевой дельтой и `EventSourceUserData`, постинг в `kCGHIDEventTap`, задержка 0.15–1 мс, Finder не реагирует.
- F3 ✅ FIFO: клик раньше маркера → `Unstable`. F3b: медленный callback → `TapDisabledByTimeout`.
- F4 ✅ Проверен только один экран (расширение — F12).

## Шаг 0b. F5–F8 — ✅ (`docs/spike-0.md`, не закоммичено)

- F5: окно Esc воспроизведено в listen-only; активный tap его закрывает; забор упорядочен.
- F6: Carbon при Secure Input приходит, tap не видит Space → отказ `SecureInputOn` через Carbon.
- F7: tap пропускает key-up Space, HID `key_state(Space)` залипает → сторож смотрит на флаг Secure Input и HID-флаги Ctrl/Option.
- F8 ✅: зажатая кнопка видна и по HID, и по учёту tap.

## Шаг 0c. Новые проверки платформы (до кода)

Spike получает новые env:
- `ORBIT_SPIKE_SECURE_PULSE=1` — spike сам включает Secure Input (`EnableSecureEventInput`) через 200 мс после нажатия и выключает его на Carbon `Released`;
- `ORBIT_SPIKE_NO_POLL=1` — сторож выключен;
- логирование HID-счётчиков, `eventsOfInterest` и preflight-значений.

| № | Проверка | Как | Критерий |
|---|---|---|---|
| F7b | Space-up потерян, Secure Input короче периода опроса | `SECURE_PULSE` + `NO_POLL`: нажать, отпустить Space (Secure Input выключается сразу), затем отпустить Ctrl | Tap не видел Space-up. HID-счётчик KeyUp вырос. Проверка непрерывности даёт mismatch → `Cancel`. Без `NO_POLL` — то же самое |
| F7c | Ложные mismatch без Secure Input | 50 жестов: разный порядок отпускания, быстрые повторы, autorepeat, набор текста до и сразу после | Ни одного mismatch. Синтетические клавиши других приложений (если есть) учитываются одинаково |
| F9 | Матрица разрешений | По очереди: нет ни одного разрешения; только Accessibility; только Input Monitoring; оба. Отзыв и возврат на ходу. Между прогонами `tccutil reset` | Для каждого случая: создан ли tap, `eventsOfInterest`, `enabled`, preflight-значения, какие типы событий реально приходят, работает ли поглощение, приходит ли Carbon, нужен ли перезапуск |
| F10 | Порядок tap → Carbon | `TAP_DELAY 300`; 30 нажатий, в том числе несколько Space при удержанных Ctrl+Option | Carbon `Pressed` всегда приходит после callback tap для того же key-down. Число Carbon `Pressed` равно числу key-down хоткея без autorepeat |
| F11 | Маркеры | Активный tap поглощает маркер. Поздний маркер (после таймаута) тоже поглощается. Маркер, отправленный из callback tap, встаёт в FIFO после текущего события | Finder маркеров не видит, порядок сохраняется |
| F12 | Экраны | Второй монитор сверху и слева от основного; разный scale (Retina + 1x); фокус клавиатуры на втором экране (`NSScreen.main` ≠ основной) | Точка из tap после пересчёта совпадает с `NSEvent.mouseLocation` |

**Если F7b не подтвердит счётчики** (под Secure Input HID-счётчики не растут), запасной вариант такой:
- действие фиксируется только на увиденном tap key-up Space;
- при отпускании Ctrl/Option первым жест ждёт Space-up не дольше T, иначе `Cancel`.

У этого варианта известная дыра: Space повторно нажат под Secure Input. Поэтому при провале F7b возвращаюсь к тебе и этап не начинаю.

## Итоги шага 0c (28de4a1) — поправки к плану

- **F7b ✅** Проверка непрерывности ловит Space-up под Secure Input (со сторожем и без). Запасной вариант не нужен.
- **F7c ✅** `keyboard_seen` считает только KeyDown/KeyUp **без autorepeat**: HID-счётчики автоповтор не считают.
- **F9 — права:**
  - Активному tap нужно **только Accessibility**. С одним Input Monitoring активный tap не создаётся. С одним Accessibility маска полная и маркеры постятся, хотя preflight Listen/Post = false.
  - Готовность = tap `enabled` + `eventsOfInterest ⊇ REQUIRED_MASK` + `AXIsProcessTrusted`. Preflight Listen/Post не используются.
  - Таблица `carbon_pressed`: строка «Нет Input Monitoring → `InputMonitoringDenied`» удаляется. Вариант `ResolveError::InputMonitoringDenied` в orbit-app остаётся (публичный API), macOS-адаптер его не выдаёт.
  - **P1: отзыв Accessibility при активном tap останавливает весь ввод** (~10 с до `TapDisabledByTimeout`, повторное включение зацикливает; понадобилась перезагрузка). Поэтому:
    1. таймер (250 мс) проверяет `AXIsProcessTrusted` и при потере удаляет tap: `CFMachPort::invalidate` + стоп run loop + `record_interrupt` + сигналы трекера `Cancel`/`Release`. Вызов занимает до 22 мс, поэтому проверку делаем не в потоке tap;
    2. на `TapDisabledBy*` tap включается снова, только если есть Accessibility и отключений не больше 3 за 10 с; иначе удаление;
    3. пока tap не работает, тот же таймер пересоздаёт его после выдачи права. Тогда первое нажатие после выдачи не теряется.
  - Accessibility, выданное на ходу, действует без перезапуска.
  - S7 в ручной матрице: отзыв права проверять только со страховочным монитором, который убивает приложение на первом `event tap disabled`.
- **F10 ✅** Carbon `Pressed` приходит только после возврата callback tap (+300 мс при задержке 300 мс). Сверка один к одному надёжна.
- **F11 ✅** Все маркеры Orbit поглощаются, включая поздние. Подтверждение до `wait` не теряется.
- **F12 ⏳** Нет второго монитора, переносится в S14.

## Следующий шаг: 1 — API orbit-app

Места правки (`pressed_at` встречается в 3 крейтах):
- `crates/orbit-app/src/messages.rs:69`: убрать `pressed_at` из `AppEvent::Invoked`; добавить `AppEvent::InvokeRefused { reason }`.
- `messages.rs:141`: убрать `pressed_at` из `Effect::ResolveTarget`.
- `messages.rs:205`: рядом с `InputMonitoringDenied` добавить `SecureInputOn`.
- `crates/orbit-app/src/ports.rs:37`: убрать `ResolveRequest.pressed_at`; документация `GestureId`.
- `crates/orbit-app/src/controller.rs:65,97,132`: убрать параметр `pressed_at`. Обработать `InvokeRefused`:
  - без удержания → `Notify(Refused)`;
  - при удержании → отброс.
- `controller.rs:631–666,1331`: тестовые хелперы. Новые тесты:
  - `InvokeRefused` без удержания и при удержании;
  - `SecureInputOn` в таблице отказов (рядом с `:1002`).
- `crates/orbit-desktop/src/runtime/effects.rs:28`, `resolver.rs:128`, `mod.rs:226`: подогнать runtime под новый API.

Проверка: fmt, clippy и test. Затем коммит без атрибуции; `.gitignore` не стейджим.

## Итоги шагов 1–2 — поправки к плану

- Шаг 1 — `0aed503`.
- Шаг 2 — `event_tap/{mod,log,tracker}.rs`:
  - Вместо `commands` трекер вызывает трейт `TapIo` (`keyboard_counter`, `button_held`, `post_marker`). Так счётчик гарантированно читается до постинга маркера.
  - `keyboard_seen` и биты кнопок хранит трекер (поток tap), а не `InputLog`.
  - `record_interrupt()` выдаёт свой seq. Иначе прерывание сразу после нажатия получило бы seq нажатия и давало бы `Clean`.
  - Потерянное отпускание (новый key-down в `Held`/`Releasing`) прерывание не записывает. Иначе испортился бы новый жест.
  - Хоткей — ровно Ctrl+Option (без Cmd/Shift), как у Carbon. Удержание — пока зажаты Ctrl и Option.
  - Ошибка постинга baseline-маркера → сразу `Cancel`, жест заканчивается на отпускании.
  - `FenceTicket` — RAII: drop без `wait` удаляет запись.
  - Маркеры: `MARKER_TAG = 0x0B17`, nonce — нижние 32 бита.

## Итоги шагов 3–7 — поправки к плану

Коммиты: `87026f3` (tap), `d221d57` (finder_target), `a579ca8` (hotkey), `4f92375` (desktop), `36efac2` (docs).

- **tap.rs.**
  - Accessibility проверяет отдельный поток `orbit-access` раз в 250 мс. При потере права он останавливает run loop tap через `CFRunLoopStop` (обёртка `TapRunLoop` с `unsafe impl Send/Sync`). Tap удаляется в своём потоке: invalidate, затем `TapDisabled` в трекер.
  - Tap пересоздаётся, когда право появляется (переход нет → есть), и через `ensure_running` на непринятом нажатии Carbon.
  - Сигналы tap уходят в callback `on_signal`, а `hotkey::forward_signals` переводит их на main.
  - Таймер сторожа стоит «на парковке» (+1 год), пока жеста нет, — поток tap не просыпается зря.
  - `TapOptions { watchdog }` нужен для `ORBIT_DEBUG_NO_POLL`.
- **permissions.rs.** Есть только `accessibility_trusted(prompt)` и `secure_input_enabled()`. `input_monitoring()` не нужен (F9).
- **finder_target.**
  - Фокус на `AXTextField`/`AXTextArea` (глубина 0) → `OutsideFileArea`. Так ловится и переименование.
  - Finder frontmost проверяется через `NSWorkspace.frontmostApplication` на потоке resolver.
  - `unready(status)` общий для resolver и `hotkey::refusal`.
- **orbit-desktop.**
  - `ORBIT_DEBUG_MAIN_DELAY_MS` действует только на `Invoked`.
  - `SECURE_PULSE` работает по таймеру: Secure Input включается через 200 мс после нажатия и выключается через 700 мс, без Carbon Released.
  - Bundle id — `com.example.orbit`.
- **Сборка.** В `[dependencies]` orbit-macos не хватало фич (NSArray и др.): их подтягивали dev-deps. Исправлено, `cargo check -p orbit-macos --lib` проходит.
- **tempfile** не добавлял: временные каталоги в тестах заводятся вручную.
- **Осталось:** ручной прогон S1–S15 вместе с пользователем (S14 — нужен второй монитор).

## Изменение публичного API (orbit-app)

- Удалить `pressed_at` из `AppEvent::Invoked`, `Effect::ResolveTarget` и `ResolveRequest`. Guard работает по seq нажатия из журнала tap.
- Новое `AppEvent::InvokeRefused { reason: ResolveError }`: хоткей нажат, но жест начать нельзя.
  - Нет удерживаемого жеста → `Notify(Refused(reason))`.
  - Есть удерживаемый жест → отброс.
  - Жест не начинает и не завершает.
- Новый вариант `ResolveError::SecureInputOn` («secure keyboard entry is on»).
- Документация `GestureId`: id выдаёт tap на key-down хоткея в своём потоке.
- I1 уточняется: точка вызова — `location` самого key-down из tap.

## Компоненты

### `orbit-macos/src/platform/event_tap/` (новый)

**`log.rs` — `InputLog`.** `Send + Sync`, `Arc`, `Mutex` + `Condvar`, без CG-типов, только номера одного потока.
- `seq: u64` монотонен и выдаётся только потоком tap.
- `last_input_seq`: mouse-down/up, `*Dragged`, key-down вне жеста.
- `last_interrupt_seq`: отключение tap, сторож, провал проверки непрерывности.
- `keyboard_seen: u64` — все KeyDown/KeyUp, которые прошли через callback (включая поглощённые).
- `hotkey_downs: u64` и `hotkey_matched: u64` — сверка нажатий один к одному с Carbon.
- `presses`: `GestureId → Press { seq, button_held }`, последние 16 жестов.
- **Забор:**
  - `register_fence() -> FenceTicket` создаёт запись `nonce → Pending` **до** постинга маркера. Записей не больше 16; переполнение → ошибка → отказ resolver.
  - `on_marker(nonce)` в tap: `Pending` → `Seen(snapshot)`; неизвестный nonce (поздний или чужой) игнорируется.
  - `wait(ticket, timeout)` удаляет запись и возвращает `Seen(snapshot) | TimedOut`. Если подтверждение пришло раньше `wait`, оно не теряется.
  - Записи переживают перезапуск tap; маркер, потерянный во время простоя, даёт `TimedOut`.
- **Метка маркера:** `EventSourceUserData = ORBIT_MAGIC << 32 | nonce`. Tap поглощает **любой** маркер с этой меткой, в том числе поздний и неизвестный.
- `check(gesture, snapshot)`:

| Условие | `Guard` |
|---|---|
| Нажатие не найдено в `presses` | `UnknownPress` |
| `button_held` | `ButtonHeld` |
| `last_input_seq > press_seq` | `Inputs` |
| `last_interrupt_seq > press_seq` | `Interrupted` |
| Ничего из этого | `Clean` |

**`tracker.rs` — чистый `GestureTracker`.**
- Вход: события, значения HID-счётчиков, маркеры, `PollState`.
- Выход: `Outcome { disposition: Pass | Swallow, signals, commands }`, где `commands` = `PostMarker(Baseline | Release)`.
- I/O (чтение счётчиков, постинг) выполняет `tap.rs`, поэтому трекер тестируется без macOS.
- Состояния: `Idle`, `Held { gesture, baseline }`, `Releasing { gesture, point, hid_at_release, deadline }`.

**Проверка непрерывности:**
1. **Invoke:** tap читает HID-счётчик `B = KeyDown + KeyUp` и отправляет baseline-маркер. Когда tap видит этот маркер, `S0 = keyboard_seen`.
2. **Триггер отпускания** (Space-up или FlagsChanged без Ctrl/Option) → `Releasing`. Tap читает `C` и отправляет release-маркер.
3. **Release-маркер:** `S1 = keyboard_seen`.
   - `C − B == S1 − S0` → `Release(held, point)`.
   - Иначе → `Cancel`, `Release` и interrupt.
   - Несовпадение в любую сторону считается отказом. Излишек возникает только из-за гонки длиной в микросекунды между чтением счётчика и постингом; это ложная отмена, то есть безопасное направление.
4. **Иначе `Releasing` завершается отменой:**
   - нет маркера за 100 мс → `Cancel` + `Release`;
   - tap отключён → `Cancel` + `Release`;
   - новый key-down хоткея → `Cancel(old)`, `Release(old)`, `Invoke(new)`.

**Переходы:**

| Событие | `Idle` | `Held` | `Releasing` |
|---|---|---|---|
| Key-down хоткея без autorepeat | `Invoke(new)` + Baseline | `Cancel(old)`, `Release(old)`, `Invoke(new)` | то же |
| Autorepeat хоткея | — | — | — |
| Key-up Space / FlagsChanged без Ctrl/Option | — | → `Releasing` + Release-маркер | — |
| Esc key-down | `Pass` | `Swallow` + `Cancel(held)`, парный key-up тоже `Swallow` | `Pass` |
| MouseMoved, `*Dragged` | — | `Move`; `*Dragged` ещё и ввод | ввод |
| Маркер Orbit | `Swallow` | `Swallow` (+ baseline) | `Swallow` (+ проверка) |
| Отключение tap | — | `Cancel`, `Release` | `Cancel`, `Release` |
| `on_poll`: Secure Input или HID-флаги без Ctrl/Option | — | `Cancel`, `Release` + interrupt | — |

**`tap.rs` — поток `orbit-event-tap`.**
- `CFRunLoop` и активный tap. Маска `REQUIRED_MASK`: KeyDown, KeyUp, FlagsChanged, Left/Right/OtherMouseDown/Up, MouseMoved, `*Dragged`, ScrollWheel.
- Callback:
  1. присвоить seq;
  2. записать событие в `InputLog`;
  3. прогнать через трекер;
  4. выполнить `commands`;
  5. отправить сигналы через `DispatchQueue::main().exec_async`;
  6. вернуть событие или `null`.
- **Готовность `status() -> TapStatus`:**
  - tap создан и `enabled`;
  - `eventsOfInterest` из `CGGetEventTapList` (запись с `tappingProcess == getpid()`) ⊇ `REQUIRED_MASK`;
  - `CGPreflightListenEventAccess()` и `AXIsProcessTrusted()`.
  - Набор условий уточняется по итогам F9.
  - `status()` пересчитывается на каждом Carbon `Pressed`.
- `TapDisabledBy*` → `record_interrupt`, сигналы трекера, `tap_enable(true)`.
- Сторож: `CFRunLoopTimer` раз в 50 мс, только в состоянии `Held`. Тот же таймер отвечает за deadline состояния `Releasing`.
- `ensure_running()` пересоздаёт tap после выдачи разрешений.

**`fence.rs`** — `post_marker(user_data)`: ScrollWheel по F2/F11. Порядок: `register_fence` → `post_marker` → `wait`. Ошибка постинга → resolver отказывает.

### `orbit-macos/src/platform/hotkey.rs` — адаптер на main
- Сигналы трекера → Cocoa-точка через `coordinates`, экраны → `sink.send(...)`.
- Carbon Ctrl+Option+Space регистрируется только для поглощения. Ошибка регистрации → выход.
- **Carbon `Pressed` → `carbon_pressed(matched, status) -> Option<ResolveError>`:**
  - `matched = log.match_hotkey()`: сверка один к одному. Если `hotkey_downs > hotkey_matched`, то `hotkey_matched += 1` и результат `true`. По F10 tap стоит раньше Carbon.
  - **Инвариант:** каждый Carbon `Pressed` даёт ровно одно — либо tap уже отправил `Invoked`, либо отправляется `InvokeRefused`.

| `matched` | Состояние | Результат |
|---|---|---|
| да | — | `None`, `debug!` |
| нет | Secure Input включён | `SecureInputOn` |
| нет | Нет Accessibility | `AccessibilityDenied` |
| нет | Нет Input Monitoring | `InputMonitoringDenied` |
| нет | Маска урезана или tap выключен | `Failed("event tap is not receiving keyboard events")` |
| нет | Иначе | `Failed("hotkey was not seen by the event tap")` |

  - Если `matched = false`, дополнительно вызывается `ensure_running()`.
  - Carbon `Released` игнорируется.

### `orbit-macos/src/platform/coordinates.rs`, `permissions.rs`
- Пересчёт Quartz → Cocoa: `y = primary.frame.height − q.y`, где `primary = NSScreen::screens()[0]` (origin (0,0), с menu bar). `NSScreen.main` не используется.
- `visibleFrame` → `ScreenRect` только для пределов размещения меню.
- Разрешения: `accessibility_trusted(prompt)`, `input_monitoring(request)`, `secure_input_enabled()`.
- Состояние ввода: `flags_state()`, `button_held()`, `keyboard_counter()` (`CGEventSourceCounterForEventType(HIDSystemState, KeyDown) + (…, KeyUp)`).

### `orbit-macos/src/platform/finder_target/` — `FinderSelectionResolver`

- **`ax.rs`** — `AxElement` из spike, timeout 0.5 с; `APIDisabled` и `CannotComplete` различаются.
- **`area.rs`** — чистая логика над `trait AxNode`: `classify`, `find_area` (≤12 уровней), `in_column_view` (1..4), desktop, `selected_items`, `item_path`.
- **`target.rs`** — `file_target(path)`:

| Случай | Результат |
|---|---|
| Путь не абсолютный | `Failed` |
| Ошибка `symlink_metadata` | `Failed("{path}: {err}")` |
| Ошибка `modified()` | `Failed` |
| Symlink | `Symlink` |
| Каталог | `Package` / `Directory` по `NSURLIsPackageKey`; ошибка ключа → `Failed` |
| Обычный файл | `File` |
| Прочее | `Failed("unsupported file type")` |

  `image: None` до этапа 8.
- **`mod.rs`** — `FinderSelectionResolver` и `factory(log)`. `resolve`:
  1. Предусловия: `status()` не готов → причина как в `carbon_pressed`; нет Accessibility → `AccessibilityDenied`; Secure Input → `SecureInputOn`.
  2. AX-чтение.
  3. Забор: `register_fence` → `post_marker` → `wait(500 мс)`. `TimedOut` → `Failed("input check did not finish")`. Повторная проверка Secure Input.
  4. `file_target(path)`.
  5. `judge(read, guard)`:
     - `AccessibilityDenied`, `InputMonitoringDenied`, `SecureInputOn` — как есть;
     - `Inputs`, `ButtonHeld`, `Interrupted` → `SelectionUnstable`;
     - `UnknownPress` → `Failed`;
     - `Clean` → результат чтения.

  Исходы чтения:

| Ситуация | Ошибка |
|---|---|
| `APIDisabled` | `AccessibilityDenied` |
| Поиск, sidebar, неизвестный список, rename-поле, нет области за 12 уровней | `OutsideFileArea` |
| 0 / >1 выделено | `NothingSelected` / `MultipleSelected` |
| Нет `AXURL`, несколько URL, не-file URL, `CannotComplete` | `Failed(причина)` |

### `orbit-desktop`
- **`instance.rs`** — `InstanceLock::acquire` через `File::try_lock`, `WouldBlock` → `AlreadyRunning`. Файл: `~/Library/Application Support/Orbit/instance.lock`.
- **`main.rs`** — порядок запуска:
  1. логи;
  2. лок;
  3. разрешения;
  4. `InputLog`;
  5. `MainThreadSender`;
  6. `Runtime::new(sink, finder_target::factory(log), UnavailableExecutor, …)`;
  7. accessory и status item;
  8. tap и `HotkeyAdapter::install`;
  9. `app.run()`.
- Debug env:
  - `ORBIT_DEBUG_RESOLVE_DELAY_MS`;
  - `ORBIT_DEBUG_MAIN_DELAY_MS`;
  - `ORBIT_DEBUG_SECURE_PULSE` и `ORBIT_DEBUG_NO_POLL` — для S12c, только в debug-сборке.
- **`stubs.rs`** — без `UnavailableResolver`.
- **`scripts/bundle.sh`** — по образцу `bundle-spike.sh`.

### Cargo
- orbit-macos `[dependencies]`: нужные features из dev-deps, плюс `global-hotkey`, `block2`, `dispatch2`.
- orbit-desktop `[dev-dependencies]`: `tempfile`.

## Порядок реализации

Каждый шаг заканчивается fmt, clippy и test и отдельным коммитом от имени пользователя. Push не делаем.

0. **Коммит шага 0b**: `tap.rs`, `main.rs` spike, `scripts/bundle-spike.sh`, `docs/spike-0.md`. `.gitignore` не стейджим.
0c. **Spike F7b–F12** вместе с тобой, итоги в `docs/spike-0.md`, коммит. Если F7b провален — стоп и решение с тобой.
1. **orbit-app**:
   - убрать `pressed_at`;
   - добавить `InvokeRefused`, `SecureInputOn`, документацию `GestureId`;
   - тесты контроллера;
   - подогнать runtime и stubs.
2. **`event_tap/log.rs` и `tracker.rs`** — чистая логика и тесты ревизий 1–3.
3. **`tap.rs`** (активный tap, готовность, сторож, маркеры), `fence.rs`, `permissions.rs`, `coordinates.rs`, features.
4. **`finder_target/`**.
5. **`hotkey.rs`**: адаптер, Carbon, `carbon_pressed`, сверка один к одному.
6. **orbit-desktop**: `instance.rs`, `main.rs`, debug env, `scripts/bundle.sh`.
7. **`docs/stage-4.md`** с матрицей S1–S15 и ручной прогон.

## Тесты (автоматические)

**Ревизия 1:**
- Чередование очередей:
  - у каждого жеста свой seq;
  - жест 2 → `Inputs`, жест 3 → `Clean`.
- Задержка tap:
  - клик до маркера → `Inputs`;
  - маркера нет → `TimedOut`;
  - маркер раньше клика → `Clean`.
- Потерянное отпускание: `[Cancel(A), Release(A), Invoke(B)]`; в контроллере нет `WriteClipboard`.

**Ревизия 2:**
- Ранний Esc:
  - `Swallow` в том же вызове, парный key-up тоже;
  - Esc без удержания → `Pass`;
  - Esc после отпускания → `Pass`.
- `InvokeRefused` при удержании → ничего.
- Сторож:
  - `on_poll(secure)` → `[Cancel, Release]` + `Interrupted`;
  - без удержания → `[]`;
  - следующий key-down → новый жест без `Cancel`.
- Drag:
  - mouse-down → Invoke → Dragged → `ButtonHeld` → `SelectionUnstable`;
  - Dragged → `Inputs`;
  - mouse-up → `Inputs`.

**Ревизия 3:**
- **Непрерывность (8):**
  - Счётчики совпали → `Release`.
  - Последовательность из замечания: Invoke, baseline-маркер, HID KeyUp +1 без события в tap, FlagsChanged без Ctrl, release-маркер → `[Cancel, Release]` + interrupt. Контроллер с курсором над «Путь» → нет `WriteClipboard`.
  - Излишек → `Cancel`.
  - Нет release-маркера за 100 мс → `Cancel`.
  - Tap отключён в `Releasing` → `Cancel`.
  - Новый key-down хоткея в `Releasing` → `Cancel`, `Release`, `Invoke`.
- **Готовность (9):**
  - Таблица `carbon_pressed`: `matched` × Secure Input × Accessibility × Input Monitoring × маска.
  - Сверка один к одному: два key-down tap до двух Carbon → оба `matched`; Carbon без key-down → отказ; поздний `Invoked` при `matched=true` не даёт двойного события.
  - Сравнение маски: `eventsOfInterest` без KeyDown → не готов.
- **Забор (10):**
  - Подтверждение до `wait` → `Seen`.
  - Два одновременных билета независимы.
  - Поздний маркер после `TimedOut` → игнор + `Swallow`.
  - Неизвестный nonce с меткой → `Swallow`.
  - Перезапуск tap: запись жива, маркер потерян → `TimedOut`; новый билет после перезапуска → `Seen`.
  - Переполнение → ошибка.
- **Координаты (11):** фейковые экраны сверху, слева, с отрицательными origin; основной экран не первый по фокусу.

**Прочее:**
- Остальные переходы и wrap id.
- `UnknownPress` после 16+ жестов, строгое `>`.
- `area.rs`, `judge` (read × guard).
- `file_target`, `InstanceLock`.

## Ручная матрица (`docs/stage-4.md`, `scripts/bundle.sh` с Apple Development)

| № | Сценарий | Ожидаемо |
|---|---|---|
| S1 | Второй запуск | «already running» |
| S2 | D1/D2/D3/D4/D5/D7/D8/D9-поиск в продукте | — |
| S3 | Отпускание до ответа resolver | Поздний результат отброшен |
| S4 | Первым отпущен Ctrl, Option или Space | `Released` |
| S5 | Esc при удержании | `GestureCancelled`, Finder Esc не получил |
| S5b | `ORBIT_DEBUG_MAIN_DELAY_MS=300`, Esc сразу после нажатия | Finder Esc не получил |
| S6 | Сектор «Путь» | `write clipboard`; без движения — ничего (I3) |
| S7 | Матрица F9 в продукте: нет разрешений / только одно / оба / отзыв / возврат на ходу | Каждое нажатие даёт либо жест, либо `InvokeRefused` с верной причиной; после возврата разрешения жест работает |
| S8 | Удержание 3 с | Один жест |
| S9 | Symlink | Цель — сама ссылка |
| S10 | `.app` | `Package` |
| S11 | Клик или стрелка во время чтения | `SelectionUnstable` |
| S12 | Secure Keyboard Entry включён, нажатие | `InvokeRefused(SecureInputOn)` |
| S12b | Secure Input включён при удержании → отпустить → выключить | Старый жест отменён без действия, следующий нормальный |
| S12c | `ORBIT_DEBUG_SECURE_PULSE=1` + `NO_POLL`: Space-up под Secure Input, затем отпустить Ctrl; курсор над «Путь» | Отмена, нет `write clipboard`; то же без `NO_POLL` |
| S13 | Маркер забора, поздний маркер | Finder не видит ни один |
| S14 | Экраны сверху и слева от основного, разный scale, фокус клавиатуры на втором экране | Точка вызова верна, меню в пределах `visibleFrame` своего экрана |
| S15 | Рамочное выделение начато до хоткея, продолжено во время чтения | `SelectionUnstable` |

## Verification
1. `cargo fmt --all -- --check`
2. `cargo clippy --workspace --all-targets --all-features -- -D warnings`
3. `cargo test --workspace --all-features`
4. `cargo build -p orbit-desktop`
5. F5–F12 записаны в `docs/spike-0.md`, S1–S15 — в `docs/stage-4.md`.

## Вне этапа
- Отрисовка меню (этап 5).
- Мгновенные действия (этап 6).
- Операции (этапы 7/8).
- UI разрешений и тексты отказов (этапы 10/11).
- Режим fast.
