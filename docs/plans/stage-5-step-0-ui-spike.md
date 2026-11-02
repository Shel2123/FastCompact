# Этап 5, шаг 0: spike кругового меню (UI)

## Context

Этап 4.5 закончен, порты `MenuView`/`ParametersView` есть, реализация — `LoggingMenuView`. Перед кодом этапа 5 (`ui/radial_panel.rs`, `ui/radial_view.rs` сейчас 1-строчные заглушки) нужно снять риски вида и скорости (цели: дорогой вид, blazing fast, минимум нагрузки; AppKit/Core Animation без SwiftUI — память `orbit-ui-stack`). Spike одноразовый, в публичный API не входит, как этап 0. Результат — таблица G1–G9 в `docs/spike-5.md` и решение по параметрам для продукта.

Известно из разведки:
- В `crates/orbit-macos/examples/spike/panel.rs` уже есть nonactivating `NSPanel` (Borderless, `NSPopUpMenuWindowLevel`, `ignoresMouseEvents`, не key), рисует `drawRect:` через `NSBezierPath`, свой `sector_at`. Фокус Finder сохраняется (D12 ✅).
- `MenuGeometry::standard()` (9 слотов, neutral 28, outer 110, порог 6), `hit`, `slot_bounds`, `place_menu` — в `orbit-domain/geometry.rs`. Ввод: `PointerMoved` идёт из tap, не из окна. View только рисует: `present(&MenuModel)`, `highlight(Highlight)`, `hide()`.
- `objc2-app-kit 0.3.2`: `NSGlassEffectView` (+`NSGlassEffectContainerView`) и `NSImage::imageWithSystemSymbolName_…` есть. **`objc2-quartz-core` в проекте нет**, `CASpringAnimation.perceptualDuration` не проверен.
- Xcode 27.1, Swift 6.4, macOS 26.7.1 — SwiftUI-эталон собрать можно.

## Что проверяем (G1–G9)

| # | Вопрос | Критерий |
|---|---|---|
| G1 | Доступен ли `objc2-quartz-core` (CALayer, CAShapeLayer, CATransaction, CASpringAnimation); есть ли `perceptualDuration/bounce` | компилируется; если нет `perceptualDuration` — `msg_send!` или пересчёт из `stiffness/damping` |
| G2 | Фон: `NSGlassEffectView` с круглой маской в nonactivating-панели; fallback `NSVisualEffectView` (`hudWindow`); внешний вид на светлом/тёмном/пёстром фоне | не квадратные углы, нет мерцания, `cornerRadius` = круг |
| G3 | Скорость показа: панель, слои, пути и иконки созданы заранее; на хоткей только `setFrame` + обновление слоёв + `orderFront` | время Carbon `Pressed` → коммит CATransaction (`os_signpost` + `CACurrentMediaTime` в лог), цель < 16 мс, ProMotion < 8 мс; 30 показов подряд, p50/p95 |
| G4 | Подсветка: смена `fillColor` готового `CAShapeLayer` в `CATransaction` 60–80 мс; неактивные приглушены | нет пропусков кадров при быстром движении по 9 секторам (Instruments Core Animation) |
| G5 | Появление: scale 0.92→1 + opacity пружиной ~150 мс; анимация не задерживает ввод (hit-test в `MenuGeometry`, не в кадрах) | подсветка сразу при движении во время появления; закрытие по `hide` без залипших слоёв |
| G6 | Иконки: SF Symbols для 9 слотов через `NSImage(systemSymbolName:)` + `SymbolConfiguration`, рендер один раз в `CGImage`/`contents` слоя; `ActionId`→имя символа | все 9 находятся; цвет по теме/доступности; кеш без аллокаций на показ |
| G7 | Окно: поведение на втором Space, над полноэкранным приложением, на рабочем столе Finder; фокус остаётся у Finder (повтор D12 для нового окна); не в Mission Control/Cmd-Tab | `panel focus check same=true`; `collectionBehavior` из spike достаточен |
| G8 | Доступность: Reduce Motion (без пружины, только fade), Reduce Transparency (непрозрачный фон), Increase Contrast, светлая/тёмная тема, акцентный цвет | меню читаемо во всех сочетаниях; смена настройки на ходу подхватывается на следующем показе |
| G9 | Нагрузка в простое: RSS и CPU при скрытой панели; никаких таймеров/CADisplayLink в простое | CPU ≈ 0 за 60 с, RSS замерить и записать (цель — минимум) |

Отдельно (G10, SwiftUI-эталон, только для подбора параметров): мини-стенд Swift (SwiftPM executable `scripts/swiftui-ref/`, не часть cargo workspace): круговое меню на SwiftUI (стекло, `.spring(duration:bounce:)`, подсветка, SF Symbols) + рантайм-дамп дерева `NSView`/`CALayer` и анимаций (`CAAnimation` keys, `duration`, `timingFunction`, spring-параметры). Цель — перенести **кривые и цвета**, а не реализацию (многие анимации SwiftUI покадровые). Сравнение на глаз рядом с objc2-версией, скорость — Instruments. Если дамп ничего не даёт — фиксируем «не удалось», берём рецепты из `orbit-ui-stack`.

## Устройство spike

- `crates/orbit-macos/examples/spike_ui/` (новый example, отдельно от `examples/spike/`, тот не трогаем): `main.rs`, `view.rs` (панель + слои), `springs.rs`/`theme.rs` по необходимости. Общее с прежним spike (хоткей, tap, `panel.rs`) — через `#[path = "../spike/…"]`, без копирования.
- Модель берётся из продукта: `MenuModel` из `orbit_app::catalog`, `MenuGeometry::standard()`, `place_menu`, `Highlight` — чтобы spike проверял ровно те типы, что пойдут в `MenuView`. Движение указателя — `PointerMoved` из tap spike; без реального Finder есть режим «свободное меню» (хоткей → меню у курсора, доступность слотов задаётся env).
- Переключатели (env, как в этапе 0): `ORBIT_SPIKE_UI_BACKGROUND=glass|vibrancy|plain`, `ORBIT_SPIKE_UI_ANIM=spring|ease|none`, `ORBIT_SPIKE_UI_DUMP=1` (дамп дерева слоёв нашей панели для сравнения со SwiftUI).
- Сборка/запуск: `scripts/bundle-spike.sh` расширить параметром example (по умолчанию `spike`, значение `spike_ui` → тот же bundle id, чтобы права Accessibility сохранились). Лог — `~/Library/Logs/Orbit/spike.log`.
- Зависимости: в `[dev-dependencies]` orbit-macos добавить `objc2-quartz-core` (0.3.2 в паре с остальными, `default-features = false` + нужные features) и `objc2-app-kit` features `NSGlassEffectView`, `NSVisualEffectView`, `NSImage`, `NSImageView`, `NSAppearance`. В продукт (`[dependencies]`) в этом шаге ничего не переезжает.
- Порядок событий и ошибки: сбой создания слоя/иконки (символ не найден) — `warn!` и пустой слой, spike не падает; показ до готовности панели невозможен (панель строится в `main` до хоткея). Все вызовы AppKit — на main (`MainThreadMarker`).

## Порядок работы

1. Проверка G1: `cargo add objc2-quartz-core` в dev-deps, hello-слой; решение по `CASpringAnimation`. Если крейт не годится — `msg_send!` обёртки (небольшие, в spike).
2. `view.rs`: панель + круглое стекло (G2) + 9 `CAShapeLayer` из `MenuGeometry::slot_bounds` + иконки (G6) + `present/highlight/hide`.
3. Анимации и подсветка (G4, G5), доступность (G8).
4. Измерения G3/G9 (signpost + лог), прогон G7 по сценариям.
5. SwiftUI-эталон и дамп (G10), сравнение параметров.
6. `docs/spike-5.md`: таблица G1–G10 с колонкой «Результат» (пустая до ручного прогона), выводы, параметры для продукта (длительности, bounce, цвета, размер, `cornerRadius`, имена символов). Код spike — одним коммитом, доки — вторым; push не делать.

## Проверка

- `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-features` (example должен проходить clippy), `scripts/check-portable.sh` не затронут (core-crates не меняются).
- Что автоматически не проверить и остаётся за пользователем: вид, плавность, задержка показа, G7 на втором Space/полноэкранном окне, Instruments. Такие строки в `docs/spike-5.md` помечать «не прогнан», пока нет результата; успех не заявлять.

## Не входит

Продуктовые `ui/radial_panel.rs` / `radial_view.rs`, `ParametersView` (форма — этап 8), поведение Shell, haptics (`NSHapticFeedbackManager` — проверить мельком в G4, если успеем; иначе отдельным пунктом этапа 5), смена порядка запуска платформы.

## Открытые вопросы

- Нужен ли SwiftUI-эталон (G10) уже сейчас, или сначала G1–G9, а эталон — по результату? Рекомендую G1–G9 первыми; G10 — после того как увидим, чего не хватает на глаз.
