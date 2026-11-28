# Этап 7: jobs, публикация без перезаписи, ZIP

Сектор ZIP создаёт архив файла или папки рядом с объектом. Он ничего не перезаписывает и читает только выбранное дерево. Долгий job виден в строке меню с процентом и кнопкой Cancel, Quit ждёт jobs до 2 с. Раньше на macOS стоял `UnavailableExecutor`: он валил любой job, а прогресс только писался в лог.

План: `~/.claude/plans/7-rosy-kay-of-8d2b904f-cdb1-4cfa-bbee-738ed6b496de.md` (ревизия 2, одобрена 2026-11-27). Отличия кода от плана записаны в файле плана под каждым шагом («Сделано 2026-11-28»), главные повторены ниже. Коммиты: `860c79a` (шаг 0), `0c0bac1`, `919f0c2` (1), `e6c99d2` (2), `338aedb` (3), `625a7e6` (4), `b7184cc` (5).

Изменения публичного API:

- новый crate `orbit-ops`: `publish::{create_temp, commit, TEMP_PREFIX, TEMP_SUFFIX}`, `archive::{archive_zip, check, ArchiveOptions, Sidecar, LocalTime}`, `executor::PortableExecutor`;
- `Feedback::job_started(&mut self, &JobSpec)` (`orbit-app/src/ports.rs`). Реализация по умолчанию пустая, поэтому Windows не менялся;
- `orbit-macos`: `ui::jobs`, `ui::jobs_indicator`, `ui::delegate::AppDelegate`, `StatusMenu` вместо функции `status_item()`. Их видит интеграционный тест `tests/main_thread`.

## Что в этапе, а что нет

| Есть | Нет (открыто) |
|---|---|
| ZIP файла, папки и пакета на macOS (`zip` crate, deflate на zlib-rs) | PNG, JPEG, Resize, Rename на macOS: серые до этапов 8 и 9 |
| Публикация без перезаписи, цикл по кандидатам имени (I5) | ZIP на exFAT: отказ с тостом, обходного пути нет (решение пользователя) |
| Обход дерева по дескрипторам: подмены ловятся, наружу ничего не читается | ZIP на Windows: нужен свой обход от дескриптора каталога |
| xattr и подписи `.app` через AppleDouble в `__MACOSX/` | Настройка метаданных macOS в UI |
| Индикатор и строки jobs в меню строки состояния, Cancel, Quit с ожиданием | Хранение несжимаемых записей без сжатия (`Stored`) |
| Backend `ditto` для диагностики (`ORBIT_ZIP_BACKEND=ditto`) | Ручная матрица Z1–Z13: не прогнана |

## Устройство

```text
orbit-domain → orbit-app → orbit-runtime → orbit-ops → orbit-macos → orbit-desktop
```

`orbit-ops` переносимый, как первые три crate'а: в нём нет `objc2` и `target_os`, только `cfg(unix)`. `scripts/check-portable.sh` проверяет и его. `orbit-windows` от `orbit-ops` пока не зависит.

| Файл | Что |
|---|---|
| `orbit-ops/src/publish.rs` | `create_temp`, `commit`: временный файл, сброс, `sync_all`, публикация под первым свободным именем |
| `orbit-ops/src/archive/mod.rs` | `archive_zip`, `check` (проверка спеки), `ArchiveOptions` |
| `orbit-ops/src/archive/tree.rs` | обход дерева по дескрипторам (`cfg(unix)`) |
| `orbit-ops/src/archive/entry.rs`, `writer.rs`, `time.rs` | метаданные записи, запись архива, DOS-время и `UT` |
| `orbit-ops/src/executor.rs` | `PortableExecutor: OperationExecutor` (время UTC, без sidecar'ов); в продукте не подключён |
| `orbit-runtime/src/jobs.rs` | worker шлёт `Indeterminate`, взяв job |
| `orbit-runtime/src/effects.rs` | `StartJob` → `feedback.job_started`, затем `jobs.start` |
| `orbit-macos/src/operations/mod.rs` | `MacExecutor`, выбор backend'а |
| `orbit-macos/src/operations/apple_double.rs` | sidecar'ы через `fcopyfile(COPYFILE_PACK)` |
| `orbit-macos/src/operations/ditto.rs`, `children.rs` | backend `ditto`; pid детей для `kill_all` |
| `orbit-macos/src/operations/local_time.rs` | `localtime_r` для DOS-поля |
| `orbit-macos/src/ui/jobs_indicator.rs` | `JobsIndicator`: чистый автомат показа jobs |
| `orbit-macos/src/ui/jobs.rs` | `Jobs`: автомат, вид, таймер порога, Quit |
| `orbit-macos/src/ui/status_item.rs` | `StatusMenu: JobsView`: значок, строки jobs, Cancel, Quit |
| `orbit-macos/src/ui/delegate.rs` | `AppDelegate`: `applicationShouldTerminate:` |
| `orbit-macos/src/ui/notice_text.rs` | «ZIP cancelled» нейтральным тоном, отказ на exFAT без префикса «failed» |

## Шаг 0. Spike

Код: `crates/orbit-macos/examples/spike_publish.rs`. По плану пример лежал бы в `orbit-ops`, но spike зовёт macOS-API (`renamex_np`, `statfs`, `localtime_r`) и меряет тома macOS, а `scripts/check-portable.sh` такое в переносимых crate'ах запрещает. Поэтому пример стоит рядом с остальными spike в `orbit-macos`. `tempfile` и `zip` пока dev-зависимости `orbit-macos`; в шаге 1 они переедут в `orbit-ops`.

```sh
cargo build -p orbit-macos --release --example spike_publish
target/release/examples/spike_publish publish <dir>          # P1, P3, P4
target/release/examples/spike_publish sync <dir> <MiB>       # P9
target/release/examples/spike_publish zip <src> <out> [lvl]  # P5–P8 (обход по пути: только для spike)
target/release/examples/spike_publish zip-pack <src> <out>   # P11: то же + AppleDouble в __MACOSX/
target/release/examples/spike_publish random <dir> <n> <MiB> # вход для P8b
target/release/examples/spike_publish fill <dir> <count>     # вход для P8c
```

Окружение прогона 2026-11-27: macOS 26.7.1, Apple Silicon, внутренний SSD. Тома HFS+, exFAT и FAT32 — образы `hdiutil create -fs …`, смонтированные `-nobrowse`. Сами образы лежат на APFS, поэтому время на них не равно времени настоящего USB-диска. SMB не подключался.

Версии из `Cargo.lock`: `tempfile` 3.27.0, `rustix` 1.1.5, `zip` 9.8.2 (`default-features = false`, `deflate-flate2-zlib-rs`), `flate2` 1.1.11, `zlib-rs` 0.6.9.

### Как tempfile публикует без перезаписи

`tempfile` 3.27.0, `src/file/imp/unix.rs`, `persist(.., overwrite = false)`:

1. `rustix::fs::renameat_with(.., RenameFlags::NOREPLACE)`. На Apple это `renameatx_np(.., RENAME_EXCL)` (`rustix` 1.1.5, `backend/libc/fs/syscalls.rs`, `renameat2` под `cfg(apple)`).
2. `ENOSYS` (запоминается в static) или `EINVAL` → откат: `hard_link(old, new)`, затем `unlink(old)`, **ошибка `unlink` игнорируется**. Значит, на откате `persist_noclobber` может вернуть `Ok`, а временное имя останется вторым именем того же файла.
3. Любая другая ошибка, включая `ENOTSUP`, возвращается вызывающему, отката нет.

На проверенных томах откат на hard link не случался ни разу: `RENAME_EXCL` либо работает, либо даёт `ENOTSUP` (таблица ниже). Будет ли он на SMB — не проверено.

### Итоги P1–P11

| # | Что | Результат |
|---|---|---|
| P1 | APFS: кандидат занят файлом, папкой, висячим symlink | ✅ Во всех трёх случаях `AlreadyExists` (errno 17), занятое не тронуто, `PersistError.file` возвращает файл, повтор с другим именем публикует. Временных файлов не осталось |
| P2 | HFS+, exFAT, FAT32 | HFS+ ✅ как APFS. FAT32 (`msdos`) ✅: `RENAME_EXCL` работает, hard link нет (`ENOTSUP`), публикация проходит. **exFAT ❌**: `RENAME_EXCL` и hard link дают `ENOTSUP` (errno 45), поэтому `persist_noclobber` в **свободное** имя падает с `ENOTSUP`; занятое имя при этом честно даёт `AlreadyExists`, перезаписи нет. С текущим контрактом ZIP на exFAT невозможен. **Решение пользователя:** I5 остаётся строгим, см. «Решения» ниже. Резерв имени через `O_CREAT|O_EXCL` на exFAT атомарен (второй даёт `EEXIST`), `rename` поверх своего пустого резерва проходит, но inode пустого файла синтетический (18446744073709551610) — такой путь отклонён. SMB: **не прогнан** |
| P3 | ASCII-имя 255 байт занято, кандидат ` 2` (257 байт) | ✅ первое → `AlreadyExists`, второе → `InvalidFilename` (`ENAMETOOLONG`, errno 63), первый файл цел. Так на всех четырёх томах |
| P4 | Права | ✅ `Builder::permissions(0o666)` при umask 022 → 0644 и у временного, и у опубликованного файла (APFS, HFS+). На exFAT и FAT32 права задаёт монтирование (0700). Временное имя `.orbit-XXXXXX.tmp` начинается с точки, Finder его не показывает |
| P5 | Скорость и размер, deflate уровня по умолчанию, прогретый кэш | Таблица ниже. Выбор: **zlib-rs** (feature `deflate-flate2-zlib-rs`): в 1,6–2 раза быстрее miniz_oxide при том же размере. Новые crate'ы: `zip`, `tempfile`, `flate2`, `zlib-rs`, `crc32fast`, `indexmap`, `hashbrown`, `equivalent`, `memchr`, `typed-path`, `zeroize`, `zeroize_derive`, `constant_time_eq`, `nix`, `cfg_aliases`, `fastrand`, `getrandom`, `rustversion`. Лицензии MIT, Apache-2.0, Zlib, Unlicense, CC0 — все разрешительные; MSRV самый высокий у `zip` — 1.89, у нас 1.97. Бинарник `orbit` (release, aarch64) вырос с 1 591 888 до 2 115 280 байт (+511 КиБ, +33 %): сравнение `860c79a~1` и `b7184cc`, в разницу входит и весь код этапа |
| P6 | Распаковка тестового дерева: `unzip`, `ditto -x -k`, `bsdtar`, Archive Utility | Структура, пустая папка, symlink (в том числе висячий), бит исполнения, права 0640 восстанавливаются везде. Кириллица: `unzip` и `ditto -x` дают байты как в архиве; `bsdtar` и Archive Utility переводят NFC-имена в NFD. С архивом самого `ditto` Archive Utility делает то же, значит, так ведёт себя и Finder; на APFS файл открывается под обоими именами. `ditto -c` не ставит в архиве флаг UTF-8, наш архив ставит. **`.app` ❌**: VLC.app после нашего архива не проходит `codesign --verify --deep --strict` ни с одним распаковщиком («code object is not signed at all» у `Contents/MacOS/plugins/*.jar`); после архива `ditto` проходит. Причина — подпись файлов не-Mach-O хранится в xattr `com.apple.cs.*` (у VLC 2035 таких файлов; такие xattr есть у 10+ приложений в `/Applications`: GIMP, Postman, JetBrains Toolbox, Arduino IDE…). Переносимый backend xattr не пишет → решено через AppleDouble, P11. Запуск распакованного приложения не проверялся (Z3) |
| P7 | Время изменения | `zip` crate пишет `UT` (0x5455) через `FullFileOptions::with_extended_timestamp`, DOS-поле — `last_modified_time(DateTime::from_date_and_time(..))` из `localtime_r`. `zipinfo -v` видит оба поля. `bsdtar` и Archive Utility берут `UT`: время точное до секунды, у symlink тоже. `unzip` берёт `UT` у файлов и папок, symlink'ам время не ставит. `ditto -x` берёт DOS-поле: нечётная секунда −1 с, symlink'ам время не ставит |
| P8 | ZIP64 | (а) один файл 5 ГиБ (разреженный): ❌ сначала — `set_auto_large_file` касается только data descriptor потокового режима, для обычного архива флаг нужен у каждой записи; с `large_file(size >= 2 ГиБ)` ✅, читают `unzip`, `bsdtar`, Python `zipfile`, `ditto -x`. Порог 2 ГиБ, а не 4: на несжимаемых данных deflate чуть больше входа, а лишние 20 байт заголовка ничего не стоят. (б) 5 записей по 1 ГиБ случайных данных, архив 5,0 ГиБ, последнее смещение 4 296 278 344: ✅ `unzip -t`, `bsdtar -t`, Python `testzip`, `ditto -x` и Archive Utility (`cmp` последней записи совпал). (в) 70 000 пустых файлов ✅ (`unzip -t`, `bsdtar`, `ditto -x`). Память: пик RSS 24,9 МиБ на 70 тыс. записей, **155,9 МиБ на 500 тыс.** в одной папке — около 300 Б на запись (центральный каталог `ZipWriter` и отсортированный список имён папки). Для цели «малый RSS» это риск на огромных деревьях; на обычных (≤ 50 тыс. записей) — ≤ 25 МиБ |
| P9 | `sync_all` (`F_FULLFSYNC`) | ✅ Нигде не упал. 1 и 64 МиБ: APFS 8 и 5 мс, HFS+ 6 и 21 мс, exFAT 5 и 6 мс, FAT32 5 и 6 мс; второй `fsync` после него — микросекунды. Время на образах ≠ время USB. SMB: **не прогнан** |
| P11 | AppleDouble от системы: `fcopyfile(fd, tmp, NULL, COPYFILE_PACK \| COPYFILE_XATTR \| COPYFILE_ACL)` на уже открытом дескрипторе, если `flistxattr(fd) > 0`; результат — запись `__MACOSX/<папка>/._<имя>`, как у `ditto --sequesterRsrc` | ✅ VLC.app напрямую (1827 sidecar'ов: xattr есть у каждого файла и папки, хотя бы `com.apple.provenance`) и внутри обычной папки рядом с `readme.txt`. Каждый архив распакован Archive Utility и `ditto -x -k`: во всех четырёх случаях `codesign --verify --deep --strict` проходит, все 2035 значений `com.apple.cs.*` совпали с `/Applications/VLC.app` побайтно, `com.apple.provenance` у `readme.txt` восстановлен. `fcopyfile` с `PACK` работает и на дескрипторе папки. Время: 2,9 с против 2,4 с без sidecar'ов (+20 %): каждый sidecar — новый анонимный временный файл; в продукте один такой файл переиспользуется. `unzip` и `bsdtar` кладут `__MACOSX/` обычными файлами — как с архивом `ditto` |
| P10 | `ditto -c -k --sequesterRsrc --keepParent src dst` | ✅ В уже созданный временный файл пишет поверх: тот же inode, файл усечён, архив читается. Ошибки: `exit 1`, строки `ditto: <путь>: <причина>` (`Cannot get the real path for source …`, `…: Permission denied`). После нечитаемого файла **идёт дальше** и в конце даёт `exit 1` и битый архив без центрального каталога; 2000 нечитаемых файлов → 2000 строк, 329 КБ stderr, больше буфера pipe (64 КБ) — stderr нужно читать во время работы. `SIGTERM` посреди работы: `exit 143`, ditto завершился, недописанный файл остаётся (его удаляет Orbit) |

P5 подробно (секунды по `/usr/bin/time -l`, размер архива, пик RSS):

| Корпус | zlib-rs | miniz_oxide | ditto |
|---|---|---|---|
| `~/.cargo/registry/src` (391 МБ, мелкие файлы) | 4,5 с, 56,6 МиБ, 11 МиБ RSS | 7,1 с, 56,3 МиБ | 12,6 с, 64,4 МиБ, 18 МиБ RSS |
| `VLC.app` (150 МБ) | 2,4 с, 57,5 МиБ, 5 МиБ RSS | 4,8 с, 56,8 МиБ | 6,3 с, 59,5 МиБ |
| 512 МиБ случайных данных | 9,8 с (52 МБ/с), 512,2 МиБ | 11,8 с | 11,4 с |

Несжимаемые данные deflate прогоняет зря (~52 МБ/с у всех трёх). Хранить такие записи без сжатия (`Stored`) — возможная оптимизация, в план этапа не входит.

### Решения пользователя по итогам spike (2026-11-27)

1. **Тома без исключающего rename (exFAT, P2).** I5 остаётся строгим. `O_CREAT|O_EXCL` резервирует имя атомарно, но последующий `rename` перезапишет файл, если кто-то подменил резерв; даже надёжная сверка inode оставляет гонку между проверкой и `rename`. Поэтому: `ENOTSUP` от `persist_noclobber` → job останавливается, временный архив удаляется, тост «Orbit can't safely create a ZIP on this disk». Текст общий: тип файловой системы Orbit не проверяет. `AlreadyExists` → следующий кандидат с тем же временным архивом.
2. **xattr и подписи `.app` (P6).** Переносимый writer пишет AppleDouble-sidecar'ы. Кодирует их система: `fcopyfile(COPYFILE_PACK)` на дескрипторе, который обход уже проверил; этот код живёт в `orbit-macos`, `orbit-ops` только пишет переданные байты под `__MACOSX/`. Writer, прогресс и отмена остаются одни. P11 — проверка перед каждым релизом. Если P11 когда-нибудь не пройдёт, запасной путь: `ditto` для пакетов **и для папок, внутри которых есть `.app`** — иначе архив родительской папки сломается так же.

## Шаг 1. Публикация

Одна точка фиксации на все backend'ы — `publish::commit(BufWriter<NamedTempFile>, dir, destination, cancelled)`:

1. `create_temp(dir)` создаёт `.orbit-XXXXXX.tmp` в каталоге назначения, права 0666 до umask (P4).
2. `commit` сбрасывает `BufWriter`. Ошибка сброса возвращается как `Io`, в `Drop` она не теряется. Затем `sync_all` (P9).
3. По кандидатам из `Destination::Candidates`: перед каждой попыткой проверяется отмена, потом `persist_noclobber`.
   - `AlreadyExists` → следующий кандидат с тем же временным файлом. Архив не пересжимается.
   - Отказ тома (`ErrorKind::Unsupported` или errno `ENOTSUP`/`EOPNOTSUPP`) → стоп, `Io { kind: Unsupported, "Orbit can't safely create a ZIP on this disk" }`. `std` не переводит `ENOTSUP` (45 на macOS) в `Unsupported`, поэтому errno проверяется через `rustix`.
   - Любая другая ошибка, в том числе `ENAMETOOLONG`, → стоп, `Io { kind, message }`. Сообщение называет действие и каталог, путь временного файла из него убран.
   - Кандидаты кончились → `NoFreeName`.
4. **Фиксация — успешный `persist_noclobber`.** До неё любая ошибка и отмена удаляют временный файл. После неё job всегда `Ok`, даже если отмена пришла одновременно: архив уже лежит под своим именем.

Обычный `rename`, проверка `exists()` и резерв имени через `O_CREAT|O_EXCL` не используются никогда. `Destination::Exact` (Rename, этап 9) здесь → `Internal`. Тесты коллизии с папкой, symlink и длинного имени есть только под `cfg(unix)`: на Windows публикация в таких случаях не проверялась.

## Шаг 2. ZIP и обход по дескрипторам

`archive::archive_zip(spec, context, options)` сам проверяет спеку (`check`): только `ArchiveZip`, действие доступно для фактов цели, `Destination::Candidates`. Иначе `Internal`: контроллер так не делает, значит, это ошибка Orbit.

**Что читается.** `O_NOFOLLOW` защищает только последний компонент пути. Поэтому после корня ничего не открывается по полному пути:

- корень: родитель открывается по пути, сам объект — `openat(parent, name, O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC)`, затем `fstat` → `FileTarget::check_source`. Корня нет на месте к старту → `SourceChanged::Replaced`;
- внутри: тип по `fstatat(.., AT_SYMLINK_NOFOLLOW)`, затем `openat` относительно дескриптора папки. Тип и `(dev, ino)` открытого дескриптора должны совпасть с `fstatat`, иначе `Replaced`. `O_NONBLOCK` — чтобы подменённый FIFO не повесил `open`;
- symlink пишется ссылкой (`readlinkat`), по нему Orbit не ходит. FIFO, socket, device пропускаются и не открываются. В другой том обход не спускается. Hard link пишется отдельной копией. Скрытые файлы входят, как у Finder;
- записи внутри папки отсортированы по имени, поэтому архив детерминирован. Имя не в UTF-8 → `Io { InvalidData }`. Нечитаемый или исчезнувший элемент → `Io` с путём;
- выбранный файл после чтения сверяется ещё раз (size, mtime) → `MetadataChanged`. Внутри папки новые и удалённые во время job записи попадают в архив такими, какими их увидел проход записи, — как у Finder;
- symlink, подменённый другим symlink между `fstatat` и `readlinkat`, пишется с новой целью. Наружу при этом ничего не читается.

Один дескриптор на уровень глубины: дерево глубже ~250 уровней при лимите 256 дескрипторов даёт `EMFILE` → `Io`. На других ОС, кроме `cfg(unix)`, `archive_zip` → `Internal`.

**Метаданные.** Права `mode & 0o777`. ZIP64 (`large_file`) у записи от 2 ГиБ (P8). mtime: DOS-поле в локальном времени (функцию `local_time` даёт платформа; до 1980 года поле прижимается к 1980-01-01) и `UT` в UTC (P7). Sidecar платформы (`Sidecar = &mut dyn FnMut(&File) -> io::Result<Option<Vec<u8>>>`) получает уже проверенный дескриптор. `Some(bytes)` пишется записью `__MACOSX/<папка>/._<имя>` сразу после объекта. Ошибка sidecar'а валит job.

**Прогресс и отмена.** Проход 1 считает сумму логических размеров, прогресс в это время `Indeterminate`. Проход 2 пишет, `Fraction` = прочитанные исходные байты / сумма, не больше 1. Отмена проверяется между записями и на каждом блоке в 256 КиБ.

**Память.** Содержимое идёт потоком, но `ZipWriter` держит около 300 Б на запись для центрального каталога (P8): 25 МиБ на 70 тыс. записей, 156 МиБ на 500 тыс. Sidecar'ы удваивают число записей. При ошибке и отмене `Drop` у `ZipWriter` дописывает центральный каталог во временный файл перед удалением: в `zip` нет способа «выбросить» архив. Лишняя работа растёт с числом записей.

ZIP64 проверен руками (`#[ignore]`-тесты): 70 001 запись и 5 ГиБ случайных данных в 5 записях (104 с, release) читаются обратно.

## Шаг 3. Жизненный цикл job для UI

Контракт записан в документации `Feedback` (`orbit-app/src/ports.rs`):

1. `job_started(&JobSpec)` — job принят runtime. Он ещё может ждать worker.
2. `job_progress(Indeterminate)` — worker взял job. Это значение идёт в sink мимо троттла 100 мс, иначе первый отчёт executor'а терялся бы.
3. Ещё `job_progress` — от executor'а, раз в 100 мс.
4. Ровно один `notify(JobSucceeded | JobFailed)` с тем же id.

Job, отменённый в очереди, и `QueueFull` прогресса не шлют: `job_started`, затем сразу `notify`. `JobFailed { Cancelled }` показывается нейтральным тостом «ZIP cancelled» (`Tone::Neutral`, `xmark.circle`, 1,5 с). У ошибок `Io { kind: Unsupported }` префикса «ZIP failed:» нет: текст и так называет, что не вышло.

## Шаг 4. Executor macOS

`MacExecutor` (`orbit-macos/src/operations`) исполняет только ZIP; `supported_actions` macOS — CopyPath, ShowInfo, Reveal, ArchiveZip. Backend выбирает `ORBIT_ZIP_BACKEND=portable|ditto`, по умолчанию `portable`; как и другие переключатели `MacOptions`, переменная читается только в debug-сборке. При старте в логе строка `executor ready zip_backend=…`.

- **Portable** — `orbit_ops::archive::archive_zip` с DOS-временем из `localtime_r` и sidecar'ами из `fcopyfile(COPYFILE_PACK | COPYFILE_XATTR | COPYFILE_ACL)`. Один анонимный scratch-файл на job, создаётся при первом объекте с xattr, перед каждым вызовом усекается. Том без xattr (`ENOTSUP`) — sidecar'а нет, это не ошибка. Объект только с ACL и без xattr sidecar'а не получает. Новые файлы на macOS почти всегда несут `com.apple.provenance`, снять его нельзя, поэтому sidecar есть почти у каждой записи (как в P11).
- **Ditto** — для разработки и диагностики, не даёт гарантий переносимого backend'а:
  - ditto открывает источник по пути и проходит по symlink, переданному как источник. Orbit сверяет источник с целью до запуска (identity, тип, size, mtime) и после выхода ditto, до публикации (identity и тип; у файла ещё size и mtime). Подмена и возврат объекта, пока ditto работает, не ловятся;
  - `--keepParent` только для папки и пакета: у файла флаг кладёт в архив имя родительской папки (`Parent/a.txt`), в P10 это не проверялось;
  - stderr читает отдельный поток, хранится хвост 4 КиБ; ненулевой код → `Io` с последней непустой строкой (`ditto: <путь>: <причина>`);
  - отмена: `try_wait` каждые 50 мс → `SIGKILL` и `wait`, временный файл удаляется. Прогресс только `Indeterminate`;
  - pid ребёнка лежит в `Children`, пока его не дождались; снятие и `kill_all` идут под одним замком, поэтому сигнал не уходит чужому процессу с тем же pid. `kill_all` нужен Quit (шаг 5).
- Отладка: `ORBIT_DEBUG_JOB_DELAY_MS` — пауза перед job, отмена её прерывает (Z9); `ORBIT_DEBUG_JOB_STALL_MS` — сон без проверки отмены (Z8b).

## Шаг 5. Прогресс, отмена, Quit на macOS

- **`JobsIndicator`** (`ui/jobs_indicator.rs`) — автомат без AppKit, время приходит параметром. Job живёт от `job_started` до своего `notify`: `Queued` → `Running(Progress)` после первого `job_progress`. Показывается, когда прошло 500 мс от `job_started`, время в очереди тоже считается. Короткий job виден только тостом. `QueueFull` (`job_started` и сразу `notify`) не оставляет ничего.
- **`Jobs`** (`ui/jobs.rs`) соединяет автомат, вид и таймеры. Делят его `MacFeedback` и делегат приложения. Таймер порога один, ставится на ближайший `deadline()`. Таймеры порога и Quit — `NSTimer` в `NSRunLoopCommonModes`, поэтому срабатывают и при открытом меню (`NSEventTrackingRunLoopMode`), и пока Quit ждёт (`NSModalPanelRunLoopMode`). Тихий ditto-job появляется по таймеру, без событий.
- **Common modes зависят от `NSApplication`.** Режимы tracking и modal panel попадают в common modes главного run loop, только когда создан `NSApplication`. Без него в common modes лишь default: события `MainThreadSender` в tracking mode не доходят (проверено в `tests/main_thread`). В приложении `NSApp` есть всегда. Тест создаёт его сам.
- **`StatusMenu`** (`ui/status_item.rs`):
  - на каждый показанный job — неактивная строка («Compressing “Docs” — waiting», «… — 42%», «Compressing “Docs”…») и Cancel под ней; ниже разделитель и Quit;
  - строки вставляются, меняют текст и удаляются на месте, меню не пересобирается и не закрывается;
  - Cancel хранит id job в `tag` и шлёт `CancelJobRequested` через sink. Цель действия — свой класс, его держит `StatusMenu`, потому что пункт меню держит цель слабо;
  - значок: «◎» без показанных jobs; template-кольцо по среднему проценту тех показанных jobs, что знают свою долю (перерисовка только при смене целого процента); `circle.dashed`, пока доли не знает никто.
- **Quit** (`ui/delegate.rs`, `applicationShouldTerminate:`):
  - без jobs выход сразу;
  - с jobs отменяются все: в очереди, ещё не показанные и запущенные уже во время ожидания. Делегат отвечает `TerminateLater`;
  - выход по последнему `notify`, но не позже чем через 2 с. По сроку `Children::kill_all` шлёт `SIGKILL` ditto-детям, затем `replyToApplicationShouldTerminate(true)`;
  - временный `.orbit-*.tmp` застрявшего job может остаться: worker не успевает его удалить (Z8b);
  - пока Quit ждёт, тосты не показываются.

## Debug-переключатели

Все читаются только в debug-сборке (`MacOptions`, `orbit-macos/src/debug.rs`).

| Что | Зачем |
|---|---|
| `ORBIT_ZIP_BACKEND=portable\|ditto` | backend ZIP; неизвестное значение → предупреждение и `portable`. В логе при старте `executor ready zip_backend=…` |
| `ORBIT_DEBUG_JOB_DELAY_MS` | пауза перед job, отмена её прерывает (Z9, Z6c) |
| `ORBIT_DEBUG_JOB_STALL_MS` | сон без проверки отмены: worker, застрявший в системном вызове (Z8b) |
| `cargo build -p orbit-macos --release --example spike_publish` | замеры шага 0 (P1–P11) |

## Third-party notices

`scripts/third_party_notices.py` правки не потребовал. Прогон 2026-11-28 против `860c79a~1`:

- `aarch64-apple-darwin`: 34 → 51 crate. Добавились `constant_time_eq`, `crc32fast`, `equivalent`, `errno`, `fastrand`, `flate2`, `getrandom`, `hashbrown`, `indexmap`, `memchr`, `nix`, `rustix`, `tempfile`, `typed-path`, `zeroize`, `zip`, `zlib-rs`. У всех есть свои файлы лицензий; предупреждения «no license files» — только у прежних crate'ов `objc2`;
- `x86_64-pc-windows-msvc`: 26 crate'ов, без изменений: `orbit-windows` не зависит от `orbit-ops`.

Лицензии новых crate'ов разрешительные: MIT, Apache-2.0, Zlib, Unlicense, CC0, MIT-0.

## Проверка

Автоматически, результат агента:

| Что | Результат |
|---|---|
| `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-features`, `scripts/check-portable.sh` | зелёные 2026-11-28 на коммите шага 6 (правились только docs): 376 тестов, 0 упавших, 2 `#[ignore]` (ZIP64), `main_thread` ok; `portable: ok` |
| Windows: `cargo clippy --target x86_64-pc-windows-msvc` для `orbit-ops`, `orbit-windows`, `orbit-desktop` | зелёный 2026-11-28 на шаге 5 |
| `tests/main_thread` (настоящий цикл событий) | таймер порога в default и tracking mode; события `MainThreadSender` в tracking mode; Quit по последнему job, по сроку в modal panel mode и без jobs; строки меню и Cancel; ответ делегата. Прогнан 5 раз подряд на шаге 5 |
| ZIP64 (`#[ignore]`) | руками на шаге 2: 70 001 запись, 5 ГиБ в 5 записях |

Ручная матрица (за пользователем, `task run`, лог `~/Library/Logs/Orbit/orbit.log`). Пункт без ответа — «не прогнан».

| № | Сценарий | Ожидается | Результат |
|---|---|---|---|
| Z1 | ZIP файла | `name.ext.zip` рядом, тост «Created», распаковывается Archive Utility | не прогнан |
| Z2 | ZIP папки с вложенными папками, symlink, пустой папкой, кириллицей | Структура, ссылки и mtime совпадают после распаковки | не прогнан |
| Z3 | ZIP `.app` с подписью в xattr (VLC.app) напрямую и внутри обычной папки | После Archive Utility и `ditto -x -k` проходит `codesign --verify --deep --strict`, `com.apple.cs.*` совпадают побайтно (P11, проверка перед каждым релизом), приложение запускается | не прогнан в продукте (spike P11 ✅, запуск не проверялся) |
| Z4 | `Docs.zip` уже есть: файл; отдельно — папка с таким именем | `Docs 2.zip`, занятое цело | не прогнан |
| Z5 | Каталог только для чтения | Тост с ошибкой, файлов нет | не прогнан |
| Z6 | Большая папка (> 0,5 с) | Кольцо в строке меню, строка с процентом, Cancel → тост «ZIP cancelled», нет `.orbit-*` | не прогнан |
| Z6b | Меню значка открыто всё время, пока идёт job и пока он заканчивается | Строка появляется по порогу, процент меняется на месте, по завершении строка исчезает, меню не закрывается и не мигает | не прогнан |
| Z6c | Три ZIP подряд при 2 workers | Третий показан «waiting», потом идёт; Cancel у него до старта → «ZIP cancelled» без прогресса | не прогнан |
| Z7 | Короткий job | Ни кольца, ни строки, только итоговый тост | не прогнан |
| Z8a | Quit во время обычного job, в том числе в очереди и до порога показа | Приложение закрылось быстрее чем за 2 с, нет `.orbit-*` | не прогнан |
| Z8b | Quit при застрявшем worker (`ORBIT_DEBUG_JOB_STALL_MS=10000`) | Выход ровно по сроку 2 с; временный файл может остаться — так задумано | не прогнан |
| Z9 | Источник удалён или заменён во время `ORBIT_DEBUG_JOB_DELAY_MS` | Тост «replaced/modified», архива нет | не прогнан |
| Z10 | Два ZIP одного объекта подряд | `Docs.zip` и `Docs 2.zip` | не прогнан |
| Z11 | exFAT (образ или USB): свободное имя; отдельно — `Docs.zip` уже есть | Тост «Orbit can't safely create a ZIP on this disk»; существующий архив цел; нового ZIP и `.orbit-*` нет | не прогнан в продукте (spike P2 ✅) |
| Z12 | `ORBIT_ZIP_BACKEND=ditto`: обычный ZIP, отмена, Quit по сроку | Архив с `__MACOSX`; отмена убивает ditto, временного файла нет; после Quit по сроку ditto-процесса нет (`pgrep ditto`) | не прогнан |
| Z13 | Файл, папка, картинка | PNG, JPEG, Resize, Rename серые, отпускание над ними ничего не делает | не прогнан |

## Открытое и ограничения

- Ручная матрица Z1–Z13 не прогнана.
- SMB не проверялся: ни исключающий rename (P2), ни `sync_all` (P9). Откат `tempfile` на hard link при `ENOSYS`/`EINVAL` игнорирует ошибку `unlink`: на таком томе временное имя может остаться вторым именем архива.
- exFAT: ZIP невозможен, это решение пользователя (I5 строгий).
- Windows: ZIP не подключён. Нужен свой обход от дескриптора каталога. Тесты публикации на коллизии с папкой, symlink и длинное имя есть только под `cfg(unix)`.
- Backend `ditto` не ловит подмену источника, пока ditto работает; он только для диагностики.
- Quit по сроку: временный `.orbit-*.tmp` застрявшего job может остаться в каталоге назначения.
- Память растёт с числом записей: около 300 Б на запись, вдвое больше с sidecar'ами. Отмена большого архива дописывает центральный каталог перед удалением.
- Дерево глубже ~250 уровней → `EMFILE`.
- Несжимаемые данные deflate прогоняет зря (~52 МБ/с); `Stored` для них — возможная оптимизация.
- Настройки метаданных macOS в UI нет: sidecar'ы пишутся всегда.
- `PortableExecutor` (UTC, без sidecar'ов) в продукте не подключён; его возьмёт будущая платформа без своего executor'а.
