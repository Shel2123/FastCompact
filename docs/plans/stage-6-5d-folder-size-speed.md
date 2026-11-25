# Этап 6, шаг 5d: быстрый подсчёт размера папки (ред. 2, перенесено из `memoized-whistling-cook.md`)

## Сверка с кодом на 2026-11-25

План написан 2026-11-23 (`~/.claude/plans/memoized-whistling-cook.md`), не реализован. Проверено, что код с тех пор не менялся:
- `crates/orbit-macos/src/platform/folder_size.rs` — тот же однопоточный обход из `b68ec17` (стек + `read_dir`, `PROGRESS_EVERY = 256`);
- `crates/orbit-macos/src/ui/info_size.rs:76-80` — единственный вызывающий, свой ограничитель `next` + `UPDATE_EVERY = 250 мс`;
- `FolderSize` используется ещё в `ui/info_sheet.rs:14` — тип не меняется;
- `libc 0.2.191` есть в `Cargo.lock`, прямой зависимостью `orbit-macos` не является;
- `docs/plans/` существует (stage-3/4/5), файла 5d нет;
- мерж Windows (PR #1) и beta-релиз этот код не трогали; `orbit-macos` — только macOS.

Отличий от ред. 2 нет, план идёт как есть.

## Context

5c (`b68ec17`) считает размер папки в одном потоке: `read_dir` и `fstatat` на каждую запись. Пользователь просит ускорить. Замеры 2026-11-23 на этой машине (release, кэш прогрет, 12 ядер):

| папка | записей | сейчас | 2 потока | 4 потока | 8 потоков | `du -sx` |
|---|---|---|---|---|---|---|
| /Applications | 685 тыс. | 14,7 с | 7,9 с | 4,8 с | 4,1 с | 9,1 с |
| ~/dev | 979 тыс. | 23,1 с | 11,7 с | 6,9 с | 5,7 с | 15,4 с |

`du` в одном потоке быстрее нас: `fts` в Apple libc читает папки через `getattrlistbulk`, то есть получает много записей за один вызов.

**Баг.** На `~/Library` подсчёт висел больше 10 минут в `__getdirentries64` на папке iCloud Drive (`Mobile Documents/…Notability…`). Это dataless-папка: её чтение заставляет iCloud тянуть список файлов из сети.

Выбор пользователя: «Потоки + getattrlistbulk». getattrlistbulk остаётся, только если на замере даёт ≥ 1,3× поверх потоков.

## Изменения

### 0. Сохранить план

Первым делом план копируется в `docs/plans/stage-6-5d-folder-size-speed.md`. Коммитится вместе с кодом 5d или отдельно, по команде пользователя.

`platform/folder_size.rs` → каталог `platform/folder_size/`:
- `mod.rs` — обход;
- `listing.rs` — чтение одной папки;
- `bulk.rs` — разбор буфера getattrlistbulk;
- `iopolicy.rs` — `setiopolicy_np`.

### 1. Чтение одной папки: `listing.rs`

```rust
pub(super) struct Entry { name: OsString, kind: Kind, len: u64, ino: u64, nlink: u32, flags: u32 }
pub(super) enum Listed {
    Whole,              // все записи отданы
    OtherVolume,        // папка на другом томе: не читалась
    Broken(io::Error),  // ошибка после того, как часть записей уже отдана
}
pub(super) trait Lister {
    /// Открывает `dir`, сверяет том, отдаёт записи в `each`.
    fn list(&mut self, dir: &Path, device: u64,
            each: &mut dyn FnMut(Entry) -> ControlFlow<()>) -> io::Result<Listed>;
}
```

- **Граница тома.** `File::open`, затем `fstat` дескриптора (`metadata()` открытого `File`). `st_dev ≠ device` → `OtherVolume`, чтения нет. `DEVID` из bulk-записи для решения о входе не используется (у точки монтирования он может быть от родительского тома). Ключ hard link — `(st_dev папки, ino)`.
- **`BulkLister`.** Буфер 256 KiB на рабочего, переиспользуется. Цикл `getattrlistbulk(fd, …)`, пока не вернёт 0. `unsafe` только на этом вызове.
- **`StdLister`.** `read_dir` + `DirEntry::metadata` (не идёт по ссылкам), тот же `Entry` и та же проверка тома. Запасной путь и эталон для тестов.
- **Переход на std.** Bulk упал до первого `each` (`ENOTSUP`, `EINVAL`, любая ошибка первого пакета) → папка читается `StdLister` с начала. Упал после хотя бы одной записи → `Ok(Listed::Broken(err))`: сумма остаётся, папка в `unreadable`, повтора нет.
- **Ошибки.** Ошибка открытия → `Err`, папка в `unreadable`. Запись с `ATTR_CMN_ERROR ≠ 0` → `unreadable += 1`, пропуск.
- **Источник вызова** — параметр `FnMut(fd, &mut [u8]) -> io::Result<usize>`; в тестах подделка (пакеты, потом ошибка).

### 2. Разбор bulk-записи: `bulk.rs`, без `unsafe`

- **Запрашиваем.** common: `RETURNED_ATTRS | NAME | OBJTYPE | FLAGS | FILEID | ERROR` (`ATTR_CMN_ERROR = 0x20000000` свой, в libc нет); file: `LINKCOUNT | DATALENGTH`. Без `FSOPT_PACK_INVAL_ATTRS`.
- **Порядок в записи:** `u32` длина → `attribute_set_t` → `u32` ERROR (если бит в returned) → NAME (`attrreference_t`: смещение от начала самого `attrreference_t`, длина с `\0`) → OBJTYPE `u32`, FLAGS `u32`, FILEID `u64` → LINKCOUNT `u32`, DATALENGTH `off_t`. Каждое поле — только при своём бите в returned.
- **Чтение.** Срезы `&[u8]` + `from_ne_bytes`, выравнивание не предполагается. Выход длины/поля/имени за границы → `Err(InvalidData)`, без паники; после первой записи это `Broken`.
- **Нет нужного поля** (NAME, OBJTYPE, FILEID, FLAGS; у файлов и ссылок LINKCOUNT, DATALENGTH) → дочитать `fs::symlink_metadata(dir.join(name))`. Нет имени → `unreadable`.

### 3. Обход: `mod.rs`

```rust
pub fn folder_size(root: &Path, cancel: &AtomicBool, every: Duration, progress: impl FnMut(u64)) -> Walk;
fn walk(root, cancel, every, workers: usize, lister: impl Fn() -> L, progress) -> Walk;  // для тестов
```

- **Рабочие.** `min(4, available_parallelism)` (8 потоков дают ~15 % поверх 4). `thread::scope` + `Builder::new().name("orbit-folder-walk").spawn_scoped`. Каждый первым делом включает iopolicy (п. 4). Не создался ни один → обход в вызывающем потоке без `progress`; не все → работают созданные.
- **Очередь.** `Mutex<Queue { folders: Vec<PathBuf>, busy, stop }>` + `Condvar`; задача — одна папка. Конец: очередь пуста и `busy == 0`. `lock()` через `unwrap_or_else(PoisonError::into_inner)`.
- **Вход в подпапку** — `enters(kind, flags)`: `SF_DATALESS` (0x40000000) не в очередь, а в `unreadable`. Корень проверяется так же (`symlink_metadata`). Том проверяет `Lister`.
- **Сумма.** Локальный счётчик сбрасывается в общий `AtomicU64` каждые 256 записей и в конце папки (сумма растёт и внутри огромной папки). `saturating_add` везде (общий — `fetch_update`). Symlink считается своей длиной мимо учёта hard links, как в 5c. Hard links: `Mutex<HashSet<(dev, ino)>>` только для `nlink > 1`.
- **Координатор** — вызывающий поток `orbit-folder-size`: `wait_timeout(every)` на той же `Condvar`, после таймаута `progress(sum)`. `progress` остаётся `FnMut` без `Send`, первое обновление через `every`.
- **Отмена.** Рабочие проверяют `cancel` на каждой записи и перед взятием папки; увидев — бросают папку, ставят `stop`, будят всех. Координатор тоже проверяет на каждом таймауте. `Cancelled` возвращается после выхода из `thread::scope`, когда все рабочие завершились.

**Изменение API** (`folder_size` — pub в pub-модуле `platform`): новый параметр `every: Duration`, `progress` зовётся по времени, а не каждые 256 записей. Причина: общую сумму всех потоков видит только обход. Единственный вызывающий `ui/info_size.rs`: ограничитель `next` уходит, передаётся `UPDATE_EVERY`.

### 4. iCloud: `iopolicy.rs`

`setiopolicy_np(IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES=3, IOPOL_SCOPE_THREAD=1, IOPOL_MATERIALIZE_DATALESS_FILES_OFF=1)`:
- константы сверены с `sys/resource.h`; объявление своё (в libc 0.2.191 нет);
- в каждом рабочем потоке;
- dataless-объект, пропущенный проверкой флага, даёт ошибку чтения, а не скачивание;
- вызов не прошёл → `warn!`, остаётся проверка флага.

Размер dataless-файла — из метаданных (логическая длина, как в Finder). **Заменено 2026-11-26**: считается место на диске, у dataless-файла это 0 (см. «Поправка» ниже).

### 5. Зависимость, пример, документация

- `libc = "0.2"` — прямая зависимость `orbit-macos` (0.2.191 уже в `Cargo.lock`).
- `examples/folder_size.rs`: `cargo run --release --example folder_size -- PATH [std|bulk]` → время, `bytes`, `unreadable`.
- Doc модулей: потоки, граница тома по `fstat`, iCloud не скачивается (`At least …`), блокирующее чтение сетевого тома не прерывается.

**Порог.** Пункты 1–5 целиком, потом замер `std` против `bulk` при 4 рабочих: время и CPU (`/usr/bin/time`, user+sys). `bulk` < 1,3× → `BulkLister`, `bulk.rs` и их тесты не идут в коммит, сообщаю цифры.

## Тесты

Есть: 7 тестов `folder_size` — гоняются с 1 и 4 рабочими, с `StdLister` и `BulkLister`.

Новые:
- **Граница тома.** `hdiutil create -size 4m -fs HFS+` + `attach -nobrowse -mountpoint <tmp>/vol`, файл в корне вложенного тома не посчитан, `unreadable = 0`. Guard делает `detach`; нет `hdiutil` → пропуск с `eprintln!`. Плюс `Lister::list` на точке монтирования → `OtherVolume`.
- **Bulk против std на APFS.** Файлы, подпапка, symlink, hard link, пустой файл; сравнить `name`, `kind`, `len`, `ino`, `nlink`, бит dataless.
- **Синтетические записи** (`bulk.rs`): имя не UTF-8; запись с ERROR; без FILEID/DATALENGTH → дочитывание; обрезанная запись, длина за буфером, имя за записью → `InvalidData`.
- **Переход на std.** Ошибка первого пакета → записи из `StdLister`, сумма полная; 2 записи, затем ошибка → `Broken`, сумма двух сохранена, `unreadable = 1`, без повтора.
- **Прогресс внутри папки.** Поддельный `Lister`: 300 записей, ждёт на канале, пока `progress` не увидит > 0 (`recv_timeout` 5 с), дочитывает. Промежуточное > 0 и < итога.
- **Отмена.** Бесконечная очередь папок + счётчик активных; `cancel` из `progress` → `Cancelled`, активных 0.
- **Дерево.** 30 папок × 20 файлов + hard link между папками → точная сумма, ссылка один раз.
- **`saturating_add`.** Два файла `u64::MAX / 2 + 1` → `u64::MAX`.
- **`enters`.** dataless, обычная папка, не папка.

## Verification

1. `task check` (fmt, clippy `-D warnings`, test) и `scripts/check-portable.sh`.
2. Замер примера (release, прогрев) на `/Applications` и `~/dev`, `std` против `bulk`: время и CPU. Ожидание: потоки ≈ 3×, с bulk ≈ 5–6× против 14,7 с и 23,1 с. Цифры — в ответ и в память.
3. `~/Library` завершается за секунды с `unreadable > 0`; у `bird`/`fileproviderd` нет всплеска сети.
4. `task run`: `/Applications` и `.app` — байты как у `du -skx` (после поправки ниже); папка iCloud Drive — `At least …` без зависания; большая плоская папка — сумма растёт; Esc во время подсчёта — `folder size cancelled`, CPU в ноль.
5. Отдельный коммит без атрибуции Claude; push по команде. Память `orbit-stage-6-status`: 5d, цифры, баг iCloud.

## Итоги (2026-11-26)

Код по плану: `platform/folder_size/{mod,listing,bulk,iopolicy}.rs`, `examples/folder_size.rs`, `libc` в workspace. Отличия от плана:
- у координатора своя `Condvar` (`over`) на том же мьютексе: будить его на каждое добавление папок незачем;
- учёт hard links по одному `ino`: все читаемые папки лежат на томе корня (граница тома проверяется при открытии);
- папка открывается с `O_DIRECTORY | O_NOFOLLOW`; корень symlink не бывает (`finder_target/target.rs`);
- для замера есть скрытые `Reader` и `folder_size_by`;
- добавлен тест, что настоящий `getattrlistbulk` отдаёт все поля без отката на std.

Тесты: 33 в `folder_size` (7 прежних в четырёх вариантах: std/bulk × 1/4 потока), граница тома — на смонтированном `hdiutil` образе, прошёл без пропуска.

Замер (release, кэш прогрет, 4 рабочих, по два прогона, разброс < 3 %):

| папка | 5c, 1 поток | std, 4 потока | bulk, 4 потока | CPU std → bulk (user+sys) |
|---|---|---|---|---|
| /Applications | 14,7 с | 5,2 с | 3,0 с | 13,5 → 5,7 с |
| ~/dev | 23,1 с | 7,9 с | 4,8 с | 19,1 → 8,3 с |

bulk поверх потоков — 1,7× (порог 1,3×), остаётся. Итог против 5c — 4,8–4,9×. Байты std и bulk совпадают.

`~/Library`: 7,4 с, 156 unreadable (было > 10 мин в iCloud). Активность сети у `bird`/`fileproviderd` не проверялась.

Не прогнано: пункт 4 Verification (ручная проверка в приложении).

## Поправка 2026-11-26: место на диске вместо логического размера

**Баг (нашёл пользователь).** Домашняя папка показывала «At least 1.35 TB» на диске в 460 ГиБ. 1,1 ТБ из них — `Docker.raw`: разреженный диск виртуальной машины, логически 1,1 ТБ, на диске 9,3 ГБ. Логические длины складываются в больше, чем весь диск.

**Решение пользователя:** показывать только место на диске, как `du -x` и «Хранилище» macOS. Решение 5c «логический размер, как Finder» отменено.

- bulk запрашивает `ATTR_FILE_ALLOCSIZE` (все ветки файла) вместо `ATTR_FILE_DATALENGTH`; std берёт `st_blocks × 512`. Тест сверяет оба на файле с resource fork и на сжатом (`ditto --hfsCompression`) — совпадают.
- Разреженный файл считается по занятым блокам, сжатый — по сжатому размеру, dataless — 0. Клоны APFS с общими блоками считаются каждый, как в `du`.
- Блоки самих папок не считаются (`du` их считает): расхождение около 0,02 %.
- Новый тест: разреженный файл длиной 10 ГиБ даёт меньше 1 МиБ. Ожидания тестов на настоящем диске считаются через `st_blocks`.

Проверка: `~` — 254,50 ГБ против 254,56 ГБ у `du -skx` (15,5 с, 162 unreadable — папки под защитой TCC, отсюда «At least»); `/Applications` — 47 940 526 080 байт, с `du` до байта, 3,1 с.

Size у отдельного файла по-прежнему логический (`FileVersion.size` из domain). Не изменено.
