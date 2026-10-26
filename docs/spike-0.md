# Этап 0: spike платформенных рисков

Одноразовый код для проверки платформы, в публичный API не входит: `crates/orbit-macos/examples/spike/`. Что удастся, потом перенесётся в `platform/finder_target`, `platform/hotkey` и `ui/radial_panel`.

## Что проверяем
1. AX-доступ переживает пересборку при стабильной подписи.
2. Выделенный объект в области файлов Finder с фокусом определяется однозначно во всех видах и на рабочем столе; всё остальное — отказ с причиной.
3. Ввод между нажатием и чтением не подменяет цель.
4. Ввод (удержание, движение мыши, Esc с модификаторами, отпускание) работает, пока фокус у Finder, и не даёт побочных эффектов.
5. Своя панель не меняет фокус и выделение Finder.

## Режимы и контракт цели
- **default** (`selection`, MVP): цель — ровно один выделенный объект в области файлов Finder, где фокус (активное окно или рабочий стол). Курсор задаёт только место меню.
  - Область файлов: list (`AXOutline`, `AXIdentifier=ListView`), icons/gallery (`AXList` `AXCollectionList`, `IconView`/`GalleryView`), columns (`AXList` в `AXScrollArea` в `AXBrowser` `ColumnView`), рабочий стол (`AXGroup` «desktop» в `AXScrollArea` «desktop»). Признак — контейнер, а не наличие `AXURL`.
  - Отказ: Finder не frontmost; фокус в поле поиска, sidebar (даже со строками с URL), в неизвестном списке или вне списков; ничего не выделено; выделено больше одного; ввод во время чтения (`Unstable`).
  - Выделение читается один раз за жест и хранится до конца жеста.
- **fast** (`cursor`, после MVP): цель — `AXURL` элемента под курсором или потомка `AXRow`; выделение не используется. Переключение режимов — настройка, появится вместе с fast.
- Режимы не смешиваются, отката одного в другой нет.

## Запуск

```sh
scripts/bundle-spike.sh                    # ad-hoc подпись: только отладка кода
ORBIT_SIGN_IDENTITY="Apple Development: …" scripts/bundle-spike.sh
tail -f ~/Library/Logs/Orbit/spike.log
```

Скрипт делает по шагам:
- останавливает запущенный экземпляр и ждёт его завершения;
- собирает бинарник с маркером `ORBIT_SPIKE_BUILD`;
- собирает и подписывает `target/spike/Orbit Spike.app`;
- запускает ровно один экземпляр через `open`.

Бинарник напрямую из терминала не запускать: TCC тогда отнесёт права к Terminal.

Хоткей: удерживать **Ctrl+Option+Space**. Выход: пункт ◎ → Quit в menu bar.

Переменные окружения передаются в приложение через `open --env`:

| Переменная | Назначение |
|---|---|
| `ORBIT_SPIKE_MODE` | `selection` (по умолчанию) или `cursor` |
| `ORBIT_SPIKE_SELECT_DELAY_MS` | задержка перед чтением выделения (D11) |
| `ORBIT_SPIKE_APPLESCRIPT=0` | отключить сверку через JXA |
| `ORBIT_SPIKE_TAP=1` | запустить `CGEventTap` для guard (нужен Input Monitoring) |
| `ORBIT_SPIKE_HIT_DELAY_MS` | cursor: задержка перед AX hit-test (сценарий 6a) |
| `ORBIT_SPIKE_ATTR_DELAY_MS` | cursor: задержка между hit-test и обходом атрибутов (сценарий 6) |
| `ORBIT_SPIKE_BLOCK_MAIN_MS` | каждые 6 с блокировать main на столько мс (сценарий 6b) |
| `ORBIT_SPIKE_POINT=tap` | брать точку из key-down события `CGEventTap` (включает tap) |
| `ORBIT_SPIKE_FENCE` | маркер забора: `scroll` (по умолчанию), `move`, `null`, `off`; любое значение кроме `off` включает tap (этап 4, F2/F3) |
| `ORBIT_SPIKE_TAP_DELAY_MS` | tap ждёт столько мс перед записью каждого mouse-down/key-down (включает tap; F3) |
| `ORBIT_SPIKE_ACTIVE=1` | активный tap: сам ведёт удержание и поглощает Esc, Carbon Esc не регистрируется (включает tap; F5) |
| `ORBIT_SPIKE_MAIN_DELAY_MS` | main спит столько мс перед обработкой Carbon `Pressed` (F5) |

## Устройство
- **Выделение (default).** При нажатии main проверяет, что frontmost — Finder; иначе отказ без панели. Поток resolver:
  1. `AXFocusedUIElement` приложения Finder → подъём до области файлов (см. контракт). Поиск, sidebar, неизвестный список, окно → `Unsupported`.
  2. `AXSelectedRows` (list) или `AXSelectedChildren` (остальные; для icons/gallery — у списка или его `AXSectionList`).
  3. Путь каждого выделенного элемента — свой `AXURL` или `AXURL` потомков элемента; несколько разных → `Ambiguous`.
  4. 0 → `NoSelection`, 1 → `Resolved`, больше → `Multiple`. Панель показывается только при `Resolved`.
  5. **Guard.** Через 30 мс после чтения: mouse-down или key-down (кроме Space и автоповторов) после нажатия → `Unstable`. Источники: события tap (если запущен; тогда и время нажатия из tap) и `CGEventSourceSecondsSinceLastEventType` для mouse-down. Key-down по HID только логируется (автоповторы Space). Строка лога `guard`.
     **Решение для продукта (2026-10-26, по D11):** guard только на `CGEventTap`; Input Monitoring обязателен для default-режима, без него — отказ с причиной. HID-путь пропускает клавиши и берёт время нажатия из обработчика на main, поэтому как fallback не годится.
  6. **Проверка панели.** Через 100 мс после показа панели выделение читается ещё раз; строка `panel focus check same=…` сравнивает окно, роль фокуса, область и пути. На цель не влияет.
- **Сверка JXA.** Отдельный поток, `osascript -l JavaScript`: `Finder.selection()` → JSON-массив POSIX-путей → `NSJSONSerialization`. Таймаут 2 с, затем kill. Один запуск за раз; занят → `applescript busy`. Исходы: `applescript match` (с `matches`), `denied` (-1743), `timeout`, `error`. AX-результат и жесты сверку не ждут. Сравнение — число и множество путей. JXA описывает переднее окно, поэтому не судья для D1–D9.
- **Точка.** Читается первой строкой обработчика `GlobalHotKeyEvent::set_event_handler`, который вызывается синхронно на main из Carbon. Логируются Quartz-координаты (`CGEvent`), Cocoa-координаты (`NSEvent.mouseLocation`) и их конверсия через высоту главного экрана. При `ORBIT_SPIKE_POINT=tap` дополнительно логируется точка из события tap и возраст этого события.
- **Резолв (cursor).** Отдельный поток выполняет две фазы:
  1. Hit-test через system-wide `AXUIElementCopyElementAtPosition` с таймаутом 0.5 с; pid элемента должен принадлежать Finder. Панель показывается только после этой фазы.
  2. Обход от hit-элемента вверх до 12 уровней; каждый элемент логируется. Цепочка до первого контейнера (`AXList`, `AXOutline`, `AXTable`, `AXBrowser`, `AXScrollArea`, `AXWindow` и т.п.) — это элемент файла, его верхний узел — корень элемента.
     - Сначала ищется собственный `AXURL` на этой цепочке.
     - Если его нет и корень элемента — `AXRow`, ищется `AXURL` среди потомков строки: не глубже 3 уровней и не больше 40 элементов. Ровно один путь → Resolved, несколько → Ambiguous, ни одного → Unsupported.
     - Выше корня элемента поиск не поднимается. Путь не восстанавливается из `AXDocument` окна и имени.
- **Панель.** Nonactivating `NSPanel`, не key, `ignoresMouseEvents`. Подсветка сектора идёт по глобальному монитору `MouseMoved`. Сектор X фиксирован внизу (сектор 4). Сдвиг к краю экрана есть; инвариант I3 появится на этапе 1.
- **Esc.** Пока панель открыта, Esc зарегистрирован как Carbon-hotkey с масками {нет, Ctrl, Option, Ctrl+Option}, поэтому Finder его не получает. Esc с Cmd или Shift не поглощается.
- **Сессии.** Отпускание до конца чтения закрывает жест. Поздние ответы помечаются `stale selection ignored` / `stale hit-test ignored`, поздний резолв cursor — `current=false`.

## Матрица default

Все сценарии обязательны до перехода к этапу 1. D1–D9, D11, D12 проходят каждый сам по заранее известному объекту или причине отказа; JXA в них не судья.

| # | Сценарий | Ожидание | Результат |
|---|---|---|---|
| D0 | Первый жест: диалог Automation (Finder) → разрешить; затем повторить жест | до ответа `timeout`/`denied`; после разрешения `applescript match`; AX-результат не затронут | ✅ разрешено; первый вызов 1.5 с (диалог), дальше `match` |
| D1 | Выделен A в list / icons / columns / gallery, курсор над B | Resolved(A) | columns, list, icons, gallery ✅ (сессии 1–15, 20–23): кириллица, пробелы, `U+202F` в именах скриншотов, папки. Фокус: columns — `AXList` колонки (над ним 2–3 `AXScrollArea`), icons/gallery — `AXCollectionList`, list — `AXOutline` |
| D2 | Ничего не выделено | NoSelection, панели нет | ✅ list: Documents, Downloads, games, dev → `NoSelection`, панели нет; JXA `[]`, `matches=true` |
| D3 | Выделено 2+ | Multiple, панели нет | ✅ list, 2 файла → `Multiple(2)`, панели нет; JXA `matches=true` |
| D4 | Выделение на рабочем столе, фокус на столе | Resolved(объект стола) | ✅ файл и папка на столе: фокус `AXGroup` «desktop», окна нет; JXA совпадает |
| D5 | Выделение в результатах поиска, фокус в списке результатов | Resolved(реальный путь) | ✅ icons в «Searching “Documents”»: путь из подпапки `ChatGPT/finmath/` |
| D6 | Два окна, выделение в обоих, курсор над задним | Resolved(из переднего) | ✅ (сборка 20261026T074120Z, задержка 1.5 с): переднее «Documents» → `michael_macbook.conf` (1, 2); переднее «dev» → `01_eda.ipynb` (3). Окно и путь из фокуса, JXA совпал, `panel focus check same=true` |
| D7 | Активен не Finder | `refused: Finder is not frontmost`, панели нет | ✅ iTerm2 → `refused: Finder is not frontmost` |
| D8 | Columns: папка выделена в колонке 1, файл — в активной колонке 3 | Resolved(файл) | ✅ упрощённо, 2 колонки: папка `AKOSC` в колонке 1, `client.c` в колонке 2 → Resolved(`AKOSC/client.c`) (5). Фокус `AXList` активной колонки, выделенная папка слева не попала; JXA совпал, `same=true`. Первая попытка (4): клик по файлу не прошёл, фокус остался на папке → Resolved(`AKOSC`), JXA тоже `AKOSC` — не ошибка резолвера |
| D9 | Фокус в поле поиска; фокус в sidebar (в т.ч. на строке с URL) | Unsupported | поле поиска ✅ `Unsupported` (JXA при этом отдавал выделение окна — пример, почему JXA не судья); sidebar: клик по папке в sidebar сбрасывает выделение → `NoSelection`; клик по заголовку раздела не забирает фокус — Finder сообщает фокус `AXOutline` ListView, Resolved(выделенный в списке), JXA совпадает. Фокус в sidebar мышью не воспроизводится; правило отказа для sidebar остаётся защитным. Проверить через Tab при включённой «Keyboard navigation» |
| D10 | Сверка в стабильном контексте (переднее окно, фокус в списке; рабочий стол — зафиксировать, что отдаёт Finder) | AX = JXA по числу и множеству; расхождение не оправдывает нарушение D1–D9 | ✅ все 23 сравнимых жеста `matches=true`, в т.ч. `Multiple`; JXA 135–210 мс |
| D11 | `SELECT_DELAY` 1500 мс: выделен A → нажатие → во время задержки клик по B; отдельно стрелка | Unstable или A, никогда B. Прогнать с `ORBIT_SPIKE_TAP=1` и без | С tap ✅ (задержка 3 с, сборка 20261026T075133Z): клик +338 мс → `Unstable` (tap `MouseDown(Left)` и HID mouse-down), панели нет; Ctrl+Option-клик выделение меняет — AX и JXA уже видели B (`Makefile`). Стрелка +360 мс → `Unstable` (tap `KeyDown 125`); Space отпущен раньше чтения → жест закрыт, поздний результат `stale selection ignored`. Первая попытка с 1.5 с: действия после показа панели, guard молчал по делу. Без tap (сборка 20261026T075401Z): клик +484 мс → `Unstable` (HID mouse-down) ✅; стрелка +444 мс → **Resolved(B)** ❌ — `hid_key_down` замер на +444 мс (стрелка оборвала автоповтор Space), но key-down без tap только логируется: по `SecondsSinceLastEventType` автоповтор Space от другой клавиши не отличить. Без Input Monitoring guard клавиатуру не ловит |
| D12 | Любой Resolved: `panel focus check` | `same=true` | ✅ все Resolved `same=true` |
| 7, 7a, 8, 9, 10 | см. таблицу ниже | как там | |

Информационные: D11 без tap — что ловит HID; 6b (точка задаёт только место меню); 11 (край экрана).

## Матрица cursor (fast, после MVP; задел)

Результаты прогона 2026-10-24. Для перехода к этапу 1 обязательны только 7, 7a, 8, 9, 10.

| # | Сценарий | Ожидание | Обяз. | Результат |
|---|---|---|---|---|
| 1 | Finder icons / list / columns / gallery: курсор над файлом и над папкой | Resolved(тот путь) | да | ✅ над значком или именем во всех режимах. icons/gallery: `AXImage` со своим `AXURL`; columns: `AXTextField`. Над отступом элемента (`AXGroup`) → Unsupported, безопасно; разбор продолжается |
| 2 | Результаты поиска Finder | Resolved или Unsupported, не чужой путь | да | |
| 3 | Рабочий стол, несколько окон Finder, второй монитор | Resolved(путь под курсором) | да | рабочий стол ✅ (`AXImage` со своим `AXURL` в `AXGroup` «desktop»); второе окно (Documents) ✅; второй монитор — не проверено |
| 4 | Выделен A, курсор над B | B или Unsupported, никогда A | да | ✅ list: наведён невыделенный zip (`AXSelected=false`) → zip |
| 4a | List view: `dup.txt` в корне и в раскрытой `sub/`, курсор над каждым | у каждого свой путь | да | ✅ `sub/dup.txt` и `dup.txt` раздельно, в т.ч. через колонку даты |
| 4b | Файл со скрытым расширением, локализованная папка (Applications/«Программы») | реальное имя в пути | да | |
| 5 | Пустое место окна / sidebar / другое приложение поверх Finder | Unsupported | да | ✅ Dock (по pid), поле поиска, пустой рабочий стол, пустая колонка, sidebar (у строк нет URL), большое превью в gallery (`QLAXImageAndText`) |
| 6 | `ATTR_DELAY` 1500 мс: панель уже показана, курсор уведён на B | Resolved(A), не Orbit/Unsupported | да | |
| 6a | `HIT_DELAY` 1500 мс, курсор уведён на B | Resolved(A) | да | |
| 6b | `BLOCK_MAIN` 1500 мс: нажать над A во время блокировки, увести на B | снята точка A (иначе CGEventTap и повтор) | да | |
| 7 | Активное переименование в Finder → вызов → Esc | меню закрыто, переименование Finder не отменено | да | при переименовании фокус — `AXTextField`, родитель сразу `AXApplication` (окна нет) → `Unsupported`, панель не открывается; Esc неприменим. Отменяется ли переименование хоткеем — уточнить |
| 7a | Esc при удержании Ctrl+Option, после отпускания только Ctrl, только Option | меню закрыто, Finder Esc не получил | да | Esc с Ctrl+Option ✅ (52, 55), без модификаторов ✅ (57). «Только Ctrl / только Option» после правки 8 недостижимо: отпускание модификатора уже закрывает жест. Маски Esc оставлены на гонку FlagsChanged/Esc |
| 8 | Отпустить модификаторы раньше Space | Released приходит, меню закрывается | да | ❌ → исправлено: Carbon `Released` приходит только на отпускание Space; модификаторы отпущены, а панель висит (2.3 с, сессия 2) или до Esc. Теперь отпускание Ctrl или Option закрывает жест (`cause="modifier"`), включая ожидание чтения. Повтор ✅: Ctrl первым (4, 6) и Option первым (3, 5, 7) → жест закрыт за ≤1 мс после FlagsChanged; Space позже → `released while idle`; Space первым (2, 8) → `cause="space"` |
| 9 | Фокус остаётся у Finder во время жеста (`frontmost` в логе) | да | да | ✅ `frontmost=com.apple.finder` при нажатии, показе и отпускании |
| 10 | Apple Development: Quit → правка кода → пересборка → запуск; в логе новый PID и маркер | trusted=true без повторной выдачи | да | ✅ DR: identifier + CN сертификата; после правки `ax.rs` новый PID/маркер, trusted=true |
| 10a | То же с ad-hoc | наблюдение для сравнения | нет | DR ad-hoc = `cdhash`, меняется с каждой сборкой |
| 11 | Меню у края / в углу экрана | панель целиком на экране | нет | |

## Выводы

Промежуточные (2026-10-24):
- **List view.** `AXOutline` → `AXRow` (`AXOutlineRow`) → `AXCell` → `AXTextField`. У `AXTextField` ячейки имени есть свой `AXURL` и `AXFilename` с полным именем и расширением. У `AXRow`, `AXCell` и остальных колонок URL нет, поэтому через другую колонку путь берётся из `AXURL` потомка той же строки.
- **Рабочий стол.** `AXScrollArea` «desktop» → `AXGroup` «desktop» → `AXImage` со своим `AXURL`. `AXGroup` содержит много значков, поэтому поиск среди потомков разрешён только для `AXRow`.
- **icons / gallery.** `AXList` (`AXCollectionList`, `IconView`/`GalleryView`) → `AXList` (`AXSectionList`) → `AXGroup` (`AXIdentifier` = имя) → `AXImage` со своим `AXURL`.
- **columns.** `AXBrowser` → `AXScrollArea` → `AXList` → `AXGroup` → `AXTextField` со своим `AXURL`. Пустое место колонки → hit сразу на `AXList`.
- **sidebar.** `AXOutline` «sidebar» → `AXRow` → `AXCell` → `AXStaticText`; URL нет → Unsupported.
- **Отступ элемента.** Курсор над отступом `AXGroup` в icons, gallery или columns → Unsupported. Можно ли считать такой `AXGroup` одним файлом, решим после диагностического дампа его потомков.
- Изредка hit-test возвращает `AXError(-25208)` (NotImplemented) → панели нет.
- **Скорость.** Hit-test около 1 мс; панель появляется через 3 мс после нажатия; обход 5–30 мс.
- **Подпись.** Ad-hoc → DR = `cdhash`, меняется с каждой сборкой. Apple Development → DR = identifier + CN сертификата, доступ сохраняется.

_Заполняется после прогона:_
- стратегия резолва по режимам;
- способ захвата точки;
- поддерживаемые комбинации Esc;
- длительность фаз;
- способ подписи.

## Шаг 0 этапа 4: F1–F4

Проверки перед кодом продукта (план этапа 4): единственный источник событий жеста — tap, guard по номерам событий tap с забором (fence) вместо паузы. Сборка с Apple Development, как в сценарии 10.

Забор: после AX-чтения resolver постит маркер (`kCGHIDEventTap`, `EventSourceUserData` = nonce) и ждёт до 500 мс, пока tap его обработает. Ввод с номером больше номера нажатия и меньше номера маркера → `Unstable`. Маркер не увиден → `Unstable`. Старый guard по времени (пауза 30 мс) работает параллельно; строка лога `guards` показывает оба вердикта: `fence=…` и `timed=…`.

Строки лога: `tap key` (что видит tap), `carbon event` (что видит Carbon), `fence posted` / `fence seen by tap`, `guards`, `point check`.

| # | Запуск и действия | Ожидание | Результат |
|---|---|---|---|
| F1 | `ORBIT_SPIKE_TAP=1`. Выделить файл, удержать хоткей 2 с, отпустить Space первым; повторить, отпуская Ctrl первым и Option первым; в удержании нажать Esc | на каждое нажатие `tap key` space KeyDown `autorepeat=false` раньше `carbon event invoke Pressed`; при удержании одна строка `autorepeat=true`; key-up Space и FlagsChanged (Ctrl/Option) видны в `tap key`; Esc KeyDown виден, хотя Carbon его поглощает | ✅ 18 жестов: key-down хоткея в tap на 1–5 мс раньше `carbon Pressed`; одна строка `autorepeat=true` на нажатие; key-up Space и FlagsChanged Ctrl/Option видны при любом порядке отпускания; Esc KeyDown виден в tap, хотя Carbon его поглощает. Option+Space, затем Ctrl: ни Carbon, ни tap не дают не-autorepeat key-down с обоими модификаторами — жеста нет |
| F1a | Там же: приходят ли повторные `carbon event invoke Pressed` при удержании | наблюдение | Повторных `Pressed` нет. Carbon `Released` приходит только на key-up Space: при отпускании Ctrl/Option первым Carbon молчит до Space → конец жеста только по tap (FlagsChanged) |
| F2 | По очереди `ORBIT_SPIKE_FENCE=scroll`, `move`, `null`. Обычный жест в list, icons, columns, на рабочем столе. В list view с прокруткой — список не на верхней позиции | `fence seen by tap` с `latency` в единицы мс; выделение, прокрутка и курсор Finder не изменились; в `guards` `fence=clean`. Выбрать первый подходящий кандидат | `scroll` ✅: маркер увиден в 26/26 жестах, `latency` 0,15–1 мс (первый 15,7 мс; 236 мс только в очереди за задержанным вводом F3); `last_input_seq` маркер не трогает, без ввода всегда `fence=clean`. list, icons, columns, рабочий стол (`area=Desktop`). Визуально: полоса прокрутки не мелькает, список, выделение и курсор не сдвигаются. `move`/`null` не понадобились |
| F3 | `ORBIT_SPIKE_TAP_DELAY_MS=300 ORBIT_SPIKE_SELECT_DELAY_MS=3000` с выбранным маркером. Выделен A → нажатие → во время задержки клик по B; отдельно стрелка | `fence=inputs [...]` (клик/стрелка с номером меньше маркера) → `Unstable`, никогда B. `timed=` для сравнения: стрелку пропустит, если tap ещё спит | ✅ Клик B: `fence=inputs [MouseDown seq 4389]` → `Unstable`. Стрелка: `fence=inputs [KeyDown 125]` → `Unstable`. Порядок FIFO: в сессии 4 маркер ждал 236 мс за задержанным mouse-down и получил номер после него (4558 < 4559). ↓ при удержании приходит в Finder как ⌃⌥↓ (⌥↓ — к последнему файлу): Carbon стрелку не поглощает, guard её ловит |
| F3a | То же без клика | `fence=clean`, Resolved(A); `latency` ≈ задержка tap только если перед маркером в очереди был ввод | ✅ Без ввода: `fence=clean`, Resolved(A); рабочий стол — тоже `clean` |
| F3b | `TAP_DELAY_MS=300`, без `SELECT_DELAY`: щелчки мышью во время удержания | нет ли `event tap disabled` (отключение tap системой при медленном callback) | Да: при задержке 300 мс на каждый ввод система отключает tap по таймауту (`TapDisabledByTimeout`, 27 раз при обычном наборе текста). В жестах не попалось. Вывод: callback продукта не блокирует; отключение → `Interrupted` → отказ, включение заново |
| F4 | `ORBIT_SPIKE_TAP=1`, нажатие без движения мыши на разных мониторах | в `point check` `delta` ≈ (0, 0) | ✅ один экран (3024×1964): `delta=(0,0)` во всех 26 жестах, `tap_age` 1–5 мс. Несколько мониторов не проверены — нет второго |

Прогон 2026-10-26, сборка `8d760f0`, маркер `scroll`. Итог: допущения F1–F4 подтверждены, этап 4 идёт по плану с listen-only tap и маркером `scroll`.

Если F3 не подтвердится (клик B после маркера или Resolved(B)), этап 4 останавливается: обсуждаем active tap.

## Шаг 0b этапа 4: F5–F8

Проверки после ревью второй версии плана:
- ранний Esc;
- Secure Input при нажатии и во время удержания;
- drag, начатый до нажатия.

Сборка с Apple Development.

Новые строки лога:
- `tap hold start` / `tap hold end` — удержание, как его видит сам tap; в `tap hold start` есть `hid_left` и `tap_buttons`;
- `tap swallowed escape key-down/key-up`;
- `tap drag during hold`, `tap mouse-up during hold`, `tap mouse-down`;
- `input state` — таймер в run loop tap, раз в 50 мс, пишет только изменения: `secure_input`, `held`, `hid_space`, `hid_control`, `hid_option`, `hid_left`;
- `carbon event … secure_input=`.

Проверка Esc: в Terminal запустить `cat -v`. Если Esc дошёл до приложения, в выводе появится `^[`.

| # | Запуск и действия | Ожидание | Результат |
|---|---|---|---|
| F5a | `ORBIT_SPIKE_ACTIVE=1` | `event tap running active=true`; записать `listen`/`post`. Если `event tap not created` — какие права выданы. По возможности: снять Input Monitoring при включённом Accessibility и наоборот, перезапустить || ✅ Активный tap создан при Accessibility + Input Monitoring (`listen=true post=true`). Какое из двух обязательно, не проверяли (права не снимали) |
| F5b | Контроль: `ORBIT_SPIKE_TAP=1 ORBIT_SPIKE_MAIN_DELAY_MS=300` (listen-only, Carbon Esc). Terminal с `cat -v` впереди: нажать хоткей, сразу Esc, отпустить | Esc доходит до Terminal (`^[`) — окно из ревью воспроизведено || ✅ Окно воспроизведено: Esc через 249 мс после Carbon `Pressed`, пока main спит (`main thread delayed 300ms`), — `^[` в iTerm2; так же каждый Esc удержания вне Finder (Carbon Esc регистрируется только при открытой панели). Первый прогон не засчитан: был включён Secure Keyboard Entry (`secure_input=true`, tap слеп) |
| F5c | `ORBIT_SPIKE_ACTIVE=1 ORBIT_SPIKE_MAIN_DELAY_MS=300`, то же. Повторить в Finder: Esc в удержании и Esc сразу после нажатия | `tap swallowed escape key-down`/`key-up`, `^[` нет. Esc без удержания доходит как обычно || ✅ Сборка `c8c9d5e`+, `MAIN_DELAY_MS=300`: каждый Esc в удержании (iTerm2 `cat -v` и Finder, в т.ч. 3 подряд) → `tap swallowed escape key-down`/`key-up`, `^[` нет. Esc после `tap hold end` проходит. Решение принимает сам tap по своему удержанию, main не участвует → окна нет по построению |
| F5d | `ORBIT_SPIKE_ACTIVE=1 ORBIT_SPIKE_TAP_DELAY_MS=300 ORBIT_SPIKE_SELECT_DELAY_MS=3000`: повтор F3 (клик B во время чтения); без задержек — обычные жесты и набор текста | `fence=inputs` → `Unstable`; без задержек `latency` как в F2, ввод не тормозит || ✅ 3 из 3: клик B во время чтения → `fence=inputs [MouseDown …]` → отказ, хотя AX уже прочитал B (`Resolved(B)`). Маркер после задержанного ввода, `latency` 0,12–0,23 мс. `TapDisabledByTimeout` 3 раза вне жестов (как F3b). Без задержек `latency` маркера 0,14–3 мс — как в F2 |
| F6 | `ORBIT_SPIKE_ACTIVE=1`. Terminal → Secure Keyboard Entry включён, Terminal впереди: нажать хоткей, отпустить; кликнуть мышью | `input state secure_input=true`. Есть ли `carbon event invoke Pressed`? Есть ли `tap key space`/`tap hold start`? `TapDisabledByUserInput`? `tap mouse-down` есть? || Carbon `Pressed`/`Released` приходят (`secure_input=true`). Tap **не видит** key-down/key-up Space хоткея, но видит FlagsChanged (Ctrl/Option) и mouse-down. `TapDisabledBy*` нет. HID: флаги Ctrl/Option верны, `key_state(Space)` = false при нажатом Space. Esc при этом доходит до iTerm2 (`^[`): жеста нет, tap его не начал — ожидаемо. Отдельно: в 13:08:41–13:09:48 UTC при `secure_input=true` tap видел Space/fn обычного набора — не разобрано |
| F7 | Там же, Finder впереди (Secure Input выключен): удержать хоткей, не отпуская кликнуть в окно Terminal (Secure Input включается), отпустить клавиши, вернуться в Finder | `input state` с `secure_input=true`. После отпускания: `held=true` (tap не видел отпускание?), `hid_space`/`hid_control`/`hid_option` = false? Пришёл ли `tap hold end` || Удержание в Finder → клик в iTerm2 → `secure_input=true` при `held=true` через ~50 мс. Отпускание: Space key-up tap **пропустил** (Carbon `Released` пришёл), FlagsChanged увидел → `tap hold end` по модификатору. HID `key_state(Space)` залип в true на десятки секунд после отпускания — для сторожа непригоден; HID-флаги Ctrl/Option верны. Клик с ⌃⌥ в iTerm2 вызывает его собственную реакцию на модифицированный клик — артефакт сценария |
| F8 | Finder, icons view: зажать кнопку на пустом месте и тянуть рамку; не отпуская, нажать хоткей; тянуть дальше; отпустить кнопку | `tap hold start hid_left=true tap_buttons=1`; `tap drag during hold`; `tap mouse-up during hold`. Старый guard spike, скорее всего, даст `fence=clean` — это и есть P2 || ✅ Рамка в icons view, затем хоткей: `tap hold start hid_left=true tap_buttons=1` (3 из 3), `tap drag during hold`, `tap mouse-up during hold`. Старый guard spike дал `fence=clean` (P2 воспроизведён); спасло только то, что AX не дал выделение во время рамки (`no AXSelectedChildren`) → отказ |

Прогон 2026-10-26, сборка `c8c9d5e` + правки spike. Итог:
- **F5.** Активный tap поглощает Esc удержания синхронно, ревью подтверждено: окно (F5b) есть, решение (F5c) его закрывает. Порядок забора сохраняется (F5d).
- **F6.** При Secure Input Carbon `Pressed` приходит, а tap не видит Space хоткея → отказ по Carbon-пути (`SecureInputOn`).
- **F7.** Tap может пропустить отпускание Space. Сторожу нужен флаг Secure Input и HID-флаги Ctrl/Option. HID-состояние Space ненадёжно (залипает) и в сторож не идёт.
- **F8.** Зажатая кнопка при нажатии видна и по HID, и по учёту tap → `ButtonHeld`.

