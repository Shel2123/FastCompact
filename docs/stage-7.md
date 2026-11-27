# Этап 7: jobs, публикация без перезаписи, ZIP

План: `~/.claude/plans/7-rosy-kay-of-8d2b904f-cdb1-4cfa-bbee-738ed6b496de.md` (ревизия 2, одобрена 2026-11-27). Шаги 1–6 в этом файле появятся по мере работы.

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
| P5 | Скорость и размер, deflate уровня по умолчанию, прогретый кэш | Таблица ниже. Выбор: **zlib-rs** (feature `deflate-flate2-zlib-rs`): в 1,6–2 раза быстрее miniz_oxide при том же размере. Новые crate'ы: `zip`, `tempfile`, `flate2`, `zlib-rs`, `crc32fast`, `indexmap`, `hashbrown`, `equivalent`, `memchr`, `typed-path`, `zeroize`, `zeroize_derive`, `constant_time_eq`, `nix`, `cfg_aliases`, `fastrand`, `getrandom`, `rustversion`. Лицензии MIT, Apache-2.0, Zlib, Unlicense, CC0 — все разрешительные; MSRV самый высокий у `zip` — 1.89, у нас 1.97. Прирост размера бинарника — в шаге 4, когда зависимость войдёт в продукт |
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

1. **Тома без исключающего rename (exFAT, P2).** I5 остаётся строгим. `O_CREAT|O_EXCL` резервирует имя атомарно, но последующий `rename` перезапишет файл, если кто-то подменил резерв; даже надёжная сверка inode оставляет гонку между проверкой и `rename`. Поэтому: `ENOTSUP` от `persist_noclobber` → job останавливается, временный архив удаляется, тост «На этом диске Orbit не может безопасно опубликовать ZIP». Текст общий: тип файловой системы Orbit не проверяет. `AlreadyExists` → следующий кандидат с тем же временным архивом.
2. **xattr и подписи `.app` (P6).** Переносимый writer пишет AppleDouble-sidecar'ы. Кодирует их система: `fcopyfile(COPYFILE_PACK)` на дескрипторе, который обход уже проверил; этот код живёт в `orbit-macos`, `orbit-ops` только пишет переданные байты под `__MACOSX/`. Writer, прогресс и отмена остаются одни. P11 — проверка перед каждым релизом. Если P11 когда-нибудь не пройдёт, запасной путь: `ditto` для пакетов **и для папок, внутри которых есть `.app`** — иначе архив родительской папки сломается так же.
