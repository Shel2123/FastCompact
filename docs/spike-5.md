# Этап 5, шаг 0: spike кругового меню

Одноразовый код, в публичный API не входит: `crates/orbit-macos/examples/spike_ui/`. Он проверяет вид и скорость меню на тех же типах, что пойдут в продукт (`MenuGeometry::standard()`, `MENU_LAYOUT`, `Hit`). Finder, tap и контроллер в нём не участвуют: хоткей открывает меню у курсора, движение мыши подсвечивает сектор, отпускание закрывает и пишет выбранный слот в лог. Что удастся, переедет в `ui/radial_panel.rs` и `ui/radial_view.rs`.

## Запуск

```sh
scripts/bundle-spike.sh --example spike_ui          # ad-hoc подпись: только отладка
ORBIT_SIGN_IDENTITY="Apple Development: …" scripts/bundle-spike.sh --example spike_ui
tail -f ~/Library/Logs/Orbit/spike.log
```

Bundle id тот же, что у spike этапа 0 (`com.example.orbit.spike`), поэтому Accessibility не нужно выдавать заново, если приложение подписано тем же сертификатом. Хоткей — Ctrl+Option+Space.

| Переменная | Значение |
|---|---|
| `ORBIT_SPIKE_UI_BACKGROUND` | `glass` (по умолчанию), `vibrancy`, `plain` |
| `ORBIT_SPIKE_UI_ANIM` | `spring`, `ease`, `none` (по умолчанию) |
| `ORBIT_SPIKE_UI_DURATION_MS` | длительность появления, 700 |
| `ORBIT_SPIKE_UI_BOUNCE` | отскок пружины 0–1, 0,5 |
| `ORBIT_SPIKE_UI_HIGHLIGHT_MS` | смена подсветки, 70 |
| `ORBIT_SPIKE_UI_UNAVAILABLE` | слоты через запятую, нарисованные недоступными, например `5,6,7` |
| `ORBIT_SPIKE_UI_DUMP` | `1`: дерево слоёв панели в лог при старте |

## Устройство

- Панель и все слои создаются один раз в `MenuPanel::new` (на старте ~21 мс в отладочной сборке). На хоткей: `setFrameOrigin`, смена цветов существующих слоёв без неявных анимаций, короткая анимация появления, `orderFrontRegardless`.
- Фон: `NSGlassEffectView` с `cornerRadius` = внешний радиус, либо `NSVisualEffectView` (HUD) с круглой маской слоя, либо плоский диск (`CAShapeLayer`). При Reduce Transparency стеклянный вид скрывается и показывается плоский диск.
- Сектор — `CAShapeLayer` с путём из `MenuGeometry::slot_bounds` (кольцо между нейтральной зоной и внешним радиусом, зазор 0,02 рад). Подсветка — смена `fillColor` в `CATransaction` длиной `HIGHLIGHT_MS`. Акцент берётся из `controlAccentColor`, «Отмена» — `systemRedColor`.
- Иконка — белый слой с маской-символом: SF Symbol рендерится один раз через `CGImageForProposedRect` в 2× и кладётся в `mask.contents`, поэтому цвет меняется без перерисовки.
- Появление: `CASpringAnimation(perceptualDuration:bounce:)` по `transform.scale` 0,92→1 плюс `CABasicAnimation` по `opacity`. Режимы `ease` и `none` — для сравнения. При Reduce Motion анимации нет.
- Попадание считает `MenuGeometry::hit` по позиции мыши (глобальный монитор `MouseMoved`), не кадры анимации.

## Вопросы и результаты

«Не прогнан» — код написан, на глаз и в Instruments не проверено. Строки заполняет ручной прогон.

| # | Вопрос | Критерий | Результат |
|---|---|---|---|
| G1 | `objc2-quartz-core` даёт слои, транзакции и `CASpringAnimation` с `perceptualDuration/bounce` | компилируется, значения читаются | ✅ `objc2-quartz-core 0.3.2` подключён, `initWithPerceptualDuration:bounce:`, `perceptualDuration`, `bounce`, `settlingDuration` есть в привязках; пример собирается и проходит clippy. Значения пружины в рантайме — в логе `spring animation` при первом показе: не прогнан |
| G2 | Фон: стекло в nonactivating-панели, fallback vibrancy, круглая маска | не квадратные углы, нет мерцания; светлый, тёмный, пёстрый фон | ✅ `glass` (по умолчанию): круг, без квадратных углов и мерцания на светлом, тёмном и пёстром фоне (ручной прогон) |
| G3 | Скорость показа: хоткей → первый кадр | < 16 мс (ProMotion < 8 мс); 30 показов, p50/p95 | ✅ ручной прогон, 30 показов: p95 `next run loop turn after show` < 8 мс, на глаз мгновенно (точные p50/p95 не записаны; до реального кадра не измерено, Instruments не гонялся). В лог пишутся `show returned` (до возврата `show`) и `next run loop turn after show` (после коммита транзакции); от времени до реального кадра они отличаются, кадр мерить в Instruments |
| G4 | Подсветка без пропусков кадров при быстром движении по 9 секторам | Core Animation в Instruments без просадок | ✅ на глаз гладко при быстром движении по 9 секторам (ручной прогон; Instruments Core Animation не гонялся) |
| G5 | Появление: пружина против ease против без анимации; ввод не ждёт анимацию | подсветка сразу при движении во время появления | ввод ✅: при резком движении во время появления (700 мс, bounce 0,5) сектор подсвечивается сразу, не ждёт анимацию. Вид: на 150 мс разницы между spring, ease и none не заметно; движение видно только на раздутых параметрах (700 мс). Итог: по умолчанию без анимации (`none`). Пружина 700 мс / bounce 0,5 понравилась на глаз, но выбрана не была; включается `ORBIT_SPIKE_UI_ANIM=spring ORBIT_SPIKE_UI_DURATION_MS=700 ORBIT_SPIKE_UI_BOUNCE=0.5` |
| G6 | SF Symbols для 9 слотов | все 9 найдены; цвет по состоянию | ✅ после правки: резко, 9 иконок видны (ручной прогон). Находка: слои в корневом слое окна рисовались под `NSGlassEffectView` и размывались, поэтому они вынесены в отдельный `NSView`, добавленный после стекла; на всех слоях стоит `contentsScale = backingScaleFactor` (у слоёв, созданных вручную, он 1). Для продукта: рисовать поверх стекла в собственном view. Смена цвета при подсветке и читаемость на разных фонах отдельно не проверялись |
| G7 | Окно: второй Space, поверх полноэкранного окна, рабочий стол Finder; фокус остаётся у приложения под меню | панель не берёт фокус, не видна в Mission Control | не прогнан. `collectionBehavior` и уровень окна взяты из spike этапа 0 (D12 там пройден для старой панели) |
| G8 | Reduce Motion, Reduce Transparency, Increase Contrast, светлая и тёмная тема | читаемо во всех сочетаниях | не прогнан. Reduce Motion и Reduce Transparency читаются на каждом показе; Increase Contrast в коде не учтён |
| G9 | Простой: CPU и RSS при скрытой панели | CPU ≈ 0 за 60 с | не прогнан: `ps -o rss,%cpu -p $(pgrep -f "Orbit Spike.app")` |
| G10 | SwiftUI-эталон и дамп его слоёв | перенос кривых и цветов | не сделан: по решению плана сначала G1–G9 |

## Известные ограничения spike

- Цвета секторов берутся как `CGColor` в момент покраски: динамические цвета (акцент, красный) не следуют за сменой темы, пока меню открыто. Для продукта — пересчёт при `viewDidChangeEffectiveAppearance`.
- Окно не получает мышь (`ignoresMouseEvents`), позиция идёт из глобального монитора. В продукте это место займёт `PointerMoved` из tap.
- Меню не сдвигается от края экрана через `place_menu`: используется простое ограничение по `visibleFrame`, как в spike этапа 0.
- Haptics (`NSHapticFeedbackManager`) не проверялся.
