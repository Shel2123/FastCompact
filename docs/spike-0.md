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

## Устройство
- **Выделение (default).** При нажатии main проверяет, что frontmost — Finder; иначе отказ без панели. Поток resolver:
  1. `AXFocusedUIElement` приложения Finder → подъём до области файлов (см. контракт). Поиск, sidebar, неизвестный список, окно → `Unsupported`.
  2. `AXSelectedRows` (list) или `AXSelectedChildren` (остальные; для icons/gallery — у списка или его `AXSectionList`).
  3. Путь каждого выделенного элемента — свой `AXURL` или `AXURL` потомков элемента; несколько разных → `Ambiguous`.
  4. 0 → `NoSelection`, 1 → `Resolved`, больше → `Multiple`. Панель показывается только при `Resolved`.
  5. **Guard.** Через 30 мс после чтения: mouse-down или key-down (кроме Space и автоповторов) после нажатия → `Unstable`. Источники: события tap (если запущен; тогда и время нажатия из tap) и `CGEventSourceSecondsSinceLastEventType` для mouse-down. Key-down по HID только логируется (автоповторы Space). Строка лога `guard`.
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
| D6 | Два окна, выделение в обоих, курсор над задним | Resolved(из переднего) | |
| D7 | Активен не Finder | `refused: Finder is not frontmost`, панели нет | ✅ iTerm2 → `refused: Finder is not frontmost` |
| D8 | Columns: папка выделена в колонке 1, файл — в активной колонке 3 | Resolved(файл) | |
| D9 | Фокус в поле поиска; фокус в sidebar (в т.ч. на строке с URL) | Unsupported | поле поиска ✅ `Unsupported` (JXA при этом отдавал выделение окна — пример, почему JXA не судья); sidebar: клик по папке в sidebar сбрасывает выделение → `NoSelection`; клик по заголовку раздела не забирает фокус — Finder сообщает фокус `AXOutline` ListView, Resolved(выделенный в списке), JXA совпадает. Фокус в sidebar мышью не воспроизводится; правило отказа для sidebar остаётся защитным. Проверить через Tab при включённой «Keyboard navigation» |
| D10 | Сверка в стабильном контексте (переднее окно, фокус в списке; рабочий стол — зафиксировать, что отдаёт Finder) | AX = JXA по числу и множеству; расхождение не оправдывает нарушение D1–D9 | ✅ все 23 сравнимых жеста `matches=true`, в т.ч. `Multiple`; JXA 135–210 мс |
| D11 | `SELECT_DELAY` 1500 мс: выделен A → нажатие → во время задержки клик по B; отдельно стрелка | Unstable или A, никогда B. Прогнать с `ORBIT_SPIKE_TAP=1` и без | |
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
