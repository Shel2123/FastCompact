# Этап 4: жест с tap → цель из выделения Finder (без UI)

Нажатие Ctrl+Option+Space проходит путь до эффекта в логе: tap → контроллер → чтение выделения Finder → меню (пока `info!`) → действие при отпускании. Окна меню ещё нет (этап 5), поэтому результат проверяется по логу.

Что сделано и где:

| Часть | Файл |
|---|---|
| Журнал событий tap, заборы, сверка с Carbon | `crates/orbit-macos/src/platform/event_tap/log.rs` |
| Состояние жеста, проверка непрерывности, Esc, сторож | `crates/orbit-macos/src/platform/event_tap/tracker.rs` |
| Активный tap, проверка Accessibility, готовность | `crates/orbit-macos/src/platform/event_tap/tap.rs` |
| Маркеры | `crates/orbit-macos/src/platform/event_tap/fence.rs` |
| Чтение выделения Finder | `crates/orbit-macos/src/platform/finder_target/` |
| Carbon и пересчёт координат | `crates/orbit-macos/src/platform/hotkey.rs`, `coordinates.rs` |
| Второй экземпляр, сборка приложения | `crates/orbit-desktop/src/instance.rs`, `main.rs` |

## Запуск

```sh
ORBIT_SIGN_IDENTITY="Apple Development: …" scripts/bundle.sh
tail -f ~/Library/Logs/Orbit/orbit.log
```

- Бандл: `target/bundle/Orbit.app`, bundle id `com.example.orbit`. Права выдаются ему отдельно от spike.
- Нужно только **Accessibility**. Input Monitoring не нужен (spike F9).
- При первом запуске система спросит Accessibility. После выдачи tap создаётся сам, без перезапуска: в логе `accessibility granted; creating the event tap`, затем `event tap running`.
- Выход: ◎ → Quit Orbit.

Отладочные переменные (только debug-сборка, передаются через `open --env`):

| Переменная | Назначение |
|---|---|
| `ORBIT_DEBUG_RESOLVE_DELAY_MS` | resolver ждёт столько перед чтением |
| `ORBIT_DEBUG_MAIN_DELAY_MS` | main спит столько перед обработкой нажатия |
| `ORBIT_DEBUG_SECURE_PULSE=1` | Secure Input включается через 200 мс после нажатия и выключается через 700 мс |
| `ORBIT_DEBUG_NO_POLL=1` | tap без сторожа |

## Страховка для S7

Отзыв Accessibility при активном tap может остановить весь ввод системы (spike F9e). Продукт удаляет tap сам, но S7 проверять только со страховкой, которая убивает Orbit на первом признаке отключения tap. В отдельном окне терминала:

```sh
tail -n0 -F ~/Library/Logs/Orbit/orbit.log \
  | grep -m1 "event tap disabled" \
  && pkill -KILL -f "Orbit.app/Contents/MacOS/[o]rbit"
```

Шаблон `[o]rbit` попадает в процесс Orbit, но не в саму команду страховки: иначе её убьёт `scripts/bundle.sh` при перезапуске.

Ожидаемо страховка не срабатывает: при отзыве в логе `accessibility lost; removing the event tap`, затем `event tap removed`.

## Что смотреть в логе

| Событие | Строка лога |
|---|---|
| Меню показано | `present menu` |
| Выбор действия | `write clipboard`, `reveal file`, `show info`, `present parameters` |
| Меню закрыто | `hide menu` |
| Отказ | `notify notice=Refused(…)` |
| Итог чтения | `target resolved` с `guard` и `elapsed` |
| Нажатие, которое tap не видел | `hotkey press not seen by the event tap` |
| Отмена проверкой непрерывности | `the tap missed key events of the gesture; cancelling it` |
| Отмена сторожем | `watch over the gesture lost; cancelling it` |

## Ручная матрица

| № | Сценарий | Ожидаемо | Результат |
|---|---|---|---|
| S1 | Запустить второй экземпляр (`open -n target/bundle/Orbit.app`) | Второй пишет `Orbit is already running; this instance exits` и выходит; первый работает | |
| S2 | D1–D5, D7–D9 из `docs/spike-0.md`: list, icons, gallery, columns, рабочий стол; поиск, sidebar, ничего / несколько выделено | Один выделенный объект → `present menu` с его именем; остальное → `Refused(…)` с верной причиной | |
| S3 | `ORBIT_DEBUG_RESOLVE_DELAY_MS=1000`, отпустить хоткей до ответа | Ни меню, ни действия; ответ resolver отброшен | |
| S4 | Отпустить первым Ctrl, Option или Space | Каждый раз жест завершается (`hide menu`) | |
| S5 | Esc при удержании | `hide menu`, Finder Esc не получил (выделение и режим просмотра не изменились) | |
| S5b | `ORBIT_DEBUG_MAIN_DELAY_MS=300`, Esc сразу после нажатия | Finder Esc не получил | |
| S6 | Увести курсор на сектор «Путь» и отпустить; отдельно — отпустить без движения | `write clipboard` с путём; без движения — ничего (I3) | |
| S7 | Со страховкой. Матрица F9: нет прав; только Accessibility; отзыв на ходу; возврат на ходу | Нет прав → `Refused(AccessibilityDenied)`. Отзыв → tap удалён за ≤0,3 с, ввод не тормозит. Возврат → tap создан сам, первое же нажатие даёт жест | |
| S8 | Удерживать хоткей 3 с (автоповтор) | Один жест, одно `present menu` | |
| S9 | Выделить symlink | Цель — сама ссылка (`kind: Symlink`) | |
| S10 | Выделить `.app` | `kind: Package` | |
| S11 | `ORBIT_DEBUG_RESOLVE_DELAY_MS=1000`, во время чтения клик или стрелка | `Refused(SelectionUnstable)` | |
| S12 | Включить Secure Keyboard Entry (iTerm2 → Secure Keyboard Entry, фокус в iTerm2), нажать хоткей | `hotkey press not seen…` и `Refused(SecureInputOn)` | |
| S12b | Удерживать хоткей, включить Secure Input, отпустить, выключить | Старый жест отменён без действия, следующий нормальный | |
| S12c | `ORBIT_DEBUG_SECURE_PULSE=1 ORBIT_DEBUG_NO_POLL=1`: курсор на «Путь», отпустить Space в первые 0,7 с, Ctrl — позже | `the tap missed key events…`, нет `write clipboard`. То же без `NO_POLL` | |
| S13 | Несколько жестов в длинном списке, в том числе с Esc и с отказом | Finder не прокручивается и никак не реагирует на маркеры | |
| S14 | Второй монитор сверху и слева от основного, разный scale, фокус клавиатуры на втором экране (перенесённый spike F12) | Точка вызова совпадает с курсором, меню в пределах `visibleFrame` своего экрана | |
| S15 | `ORBIT_DEBUG_RESOLVE_DELAY_MS=1000`, начать рамочное выделение до хоткея и продолжить во время чтения | `Refused(SelectionUnstable)` | |

S14 требует второго монитора; пока его нет, сценарий остаётся открытым.
