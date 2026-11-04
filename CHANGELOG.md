# Changelog

## [5.41.1](https://github.com/feliperun/phai/compare/v5.41.0...v5.41.1) (2026-11-04)


### Bug Fixes

* **cli:** anchor `report cards --month` on the requested cycle ([07f1832](https://github.com/feliperun/phai/commit/07f18320f63be452e59fc5ee19b17e84a1f70e9e))
* **pluggy:** let rules see the payer name on incoming transfers ([98b9d68](https://github.com/feliperun/phai/commit/98b9d68663fc55b3bba8d88cdc71b2fb58b81492))
* **pluggy:** store English Pluggy categories under the Portuguese taxonomy ([67f2e66](https://github.com/feliperun/phai/commit/67f2e66b6b9e6123e12ba476fbe3d48d97908524))
* **sync:** read Pluggy paths from runtime config and keep account identity ([afd4b0c](https://github.com/feliperun/phai/commit/afd4b0cc3f3543899577b8e36d9f2e94ae2171e8))

## [5.41.0](https://github.com/phai-run/phai/compare/v5.40.0...v5.41.0) (2026-07-07)


### Features

* **serve:** single production daemon on port 80 with serve lock and on-demand updates ([#262](https://github.com/phai-run/phai/issues/262)) ([add2cc9](https://github.com/phai-run/phai/commit/add2cc9b7daa756728323ea2083536d19ed74a06))

## [5.40.0](https://github.com/phai-run/phai/compare/v5.39.0...v5.40.0) (2026-07-07)


### Features

* **web:** header hierarchy pass — grouped month pill, KPI rail, ghost chips ([#260](https://github.com/phai-run/phai/issues/260)) ([76cfdd9](https://github.com/phai-run/phai/commit/76cfdd9e6b5a4c1e78e0970316682886f6365b4f))

## [5.39.0](https://github.com/phai-run/phai/compare/v5.38.0...v5.39.0) (2026-07-07)


### Features

* **web:** global mode switcher, themed month in hero, scenario dropdown ([#258](https://github.com/phai-run/phai/issues/258)) ([338f7ca](https://github.com/phai-run/phai/commit/338f7ca7637a8ac4b26d12e7ba485f3f3fb268b0))

## [5.38.0](https://github.com/phai-run/phai/compare/v5.37.0...v5.38.0) (2026-07-06)


### Features

* **serve:** install the native desktop app via phai serve install ([#256](https://github.com/phai-run/phai/issues/256)) ([bd5e1fe](https://github.com/phai-run/phai/commit/bd5e1fe9d993b6d31b487bee94a88f0c4bf97b18))

## [5.37.0](https://github.com/phai-run/phai/compare/v5.36.0...v5.37.0) (2026-07-06)


### Features

* **web:** self-host fonts + COEP require-corp for WebKit isolation ([#253](https://github.com/phai-run/phai/issues/253)) ([5434665](https://github.com/phai-run/phai/commit/5434665dea51b7b02e7796b57a0840d6be2cd743))

## [5.36.0](https://github.com/phai-run/phai/compare/v5.35.0...v5.36.0) (2026-07-04)


### Features

* **serve:** open web UI as a chromeless desktop-style window ([#251](https://github.com/phai-run/phai/issues/251)) ([f427046](https://github.com/phai-run/phai/commit/f4270469f74a4c8e961ff8cfb252414e4bf24ed6))

## [5.35.0](https://github.com/phai-run/phai/compare/v5.34.1...v5.35.0) (2026-07-04)


### Features

* **web:** full-height sheet, keyboard select, frozen insert order ([8b455a9](https://github.com/phai-run/phai/commit/8b455a9af832b19eebd54895344b0d79358031e1))
* **web:** month theme, adaptive-height sheet, filter/insert wiring, undo ([d346606](https://github.com/phai-run/phai/commit/d346606976dd600b20880e5622e1aecbf5734452))
* **web:** Nubank-ultravioleta cards + Portuguese sweep ([8d5b481](https://github.com/phai-run/phai/commit/8d5b481cd17b50e470d0eef7e903251846eef015))
* **web:** per-month theme helper (accent + season glyph) ([fac2cda](https://github.com/phai-run/phai/commit/fac2cdae26303428753ede5dfb38e89abc050911))
* **web:** planilha UX overhaul + skeuomorphic flip cards ([6b26a54](https://github.com/phai-run/phai/commit/6b26a5443d73d943479b51db230da4a10ca866cd))
* **web:** polish inline forecast insert editor ([8b63fe6](https://github.com/phai-run/phai/commit/8b63fe6b541b1575ef9e5f7ad9cdd5d859280c65))
* **web:** sheet filter bar as a single row + filtros popover ([5b93d59](https://github.com/phai-run/phai/commit/5b93d59536adcc9cbdbe40916de0d45cb4d0d374))
* **web:** skeuomorphic flip credit cards with month transactions ([228de42](https://github.com/phai-run/phai/commit/228de424f16ed88fc57c2425b728c9ce0980be79))
* **web:** sticky header with sync/version, mode shortcuts, PT tabs ([c657f1f](https://github.com/phai-run/phai/commit/c657f1ff818e58c35239cd98da5b0ab57fee9f96))

## [5.34.1](https://github.com/phai-run/phai/compare/v5.34.0...v5.34.1) (2026-07-03)


### Bug Fixes

* **web:** read forecastId (camelCase) from the create-forecast response ([39cc407](https://github.com/phai-run/phai/commit/39cc4077d0ffc96bc9fd42d78eea047de58d049b))
* **web:** read forecastId from create-forecast response (inline add-row error) ([9af567b](https://github.com/phai-run/phai/commit/9af567b429efd0cf2944dfd10545aab641c3db1c))

## [5.34.0](https://github.com/phai-run/phai/compare/v5.33.1...v5.34.0) (2026-07-03)


### Features

* **web:** sheet inline-edit alignment, multi-account filter, cards redesign ([e775d3b](https://github.com/phai-run/phai/commit/e775d3b25b3e1f7522b320b7bc243e7dd280ae7a))
* **web:** sheet inline-edit alignment, multi-account filter, cards redesign ([c867bd6](https://github.com/phai-run/phai/commit/c867bd6b9526e8939d38866fec69fffaeb63921e))

## [5.33.1](https://github.com/phai-run/phai/compare/v5.33.0...v5.33.1) (2026-07-03)


### Bug Fixes

* **web:** rebuild unified sheet to match the validated mockup ([3532440](https://github.com/phai-run/phai/commit/3532440be8470c5afeb3c68f1647413208365472))
* **web:** rebuild unified sheet to match the validated mockup ([479497c](https://github.com/phai-run/phai/commit/479497c1cca30f3b4b22cb5d5237cef7d260c770))

## [5.33.0](https://github.com/phai-run/phai/compare/v5.32.0...v5.33.0) (2026-07-03)


### Features

* named planning scenarios + unified sheet (ADR-0037, ADR-0038) ([c6eee7f](https://github.com/phai-run/phai/commit/c6eee7f0e1f65df89956d40a33b03fb6aead5680))
* **web:** rich chart tooltip with scenario breakdown and per-month scenario slices ([8dbaec4](https://github.com/phai-run/phai/commit/8dbaec4b2be16392fdf3f8ee13e7302ca888fa13))

## [5.32.0](https://github.com/phai-run/phai/compare/v5.31.0...v5.32.0) (2026-07-02)


### Features

* **serve:** baseline discard and end-template endpoints for the unified sheet ([0905de4](https://github.com/phai-run/phai/commit/0905de4581e5862005a703d533fb92c70c583fda))
* **serve:** complete unified sheet baseline and scenario flows ([75b4350](https://github.com/phai-run/phai/commit/75b435012d24abb9e1f4b1215511242b4376f358))
* **web:** finish unified sheet scenario interactions ([c695500](https://github.com/phai-run/phai/commit/c6955001c0950de2d5d90435b5b0338c78cd3bc9))

## [5.31.0](https://github.com/phai-run/phai/compare/v5.30.0...v5.31.0) (2026-07-02)


### Features

* **cli:** phai scenario subcommands for named what-if planning ([a102520](https://github.com/phai-run/phai/commit/a10252078a7bef4855767faf2cafeb1a020097e2))
* **cli:** scenario promotion applies deltas to the real plan ([8c2f308](https://github.com/phai-run/phai/commit/8c2f3085d6b7b96898bce8906ce247fbcaf328e5))
* **core:** plan_scenario and plan_change persistence (ADR-0037) ([d28dd0b](https://github.com/phai-run/phai/commit/d28dd0bc026037ad5e8bb25db77b82aff8cb6a00))
* **core:** scenario projection engine (apply/diff) ([71af887](https://github.com/phai-run/phai/commit/71af887aa77c13fc0536e5c861edd46b261782b3))
* **planning:** add named what-if scenarios ([e84198c](https://github.com/phai-run/phai/commit/e84198c12a8a5c849b318d3b63e98bb293fabdd9))
* **serve:** /api/scenario endpoints for the web planner ([d5c4f9d](https://github.com/phai-run/phai/commit/d5c4f9da4a0936ad9153ff68c5e6781a36f81c9f))
* **web:** scenario mode UI — picker, chart overlay and change routing ([68784f9](https://github.com/phai-run/phai/commit/68784f9ae51a1943affdbea4e54121646486c594))
* **web:** scenario tables, events and sync routing (STORE_VERSION 11) ([7380ae1](https://github.com/phai-run/phai/commit/7380ae1a360764c577eb4bd2d9a9ba6ae78eacad))

## [5.30.0](https://github.com/phai-run/phai/compare/v5.29.0...v5.30.0) (2026-06-29)


### Features

* **serve:** auto-update detection and in-app update for CLI and WebUI ([#234](https://github.com/phai-run/phai/issues/234)) ([575a129](https://github.com/phai-run/phai/commit/575a1293693406e4b34a33d3116339045bd08526))
* **web:** QoL release — loading animation, ⌘K search, micro-animations, forecast UX consolidation ([#236](https://github.com/phai-run/phai/issues/236)) ([38952b7](https://github.com/phai-run/phai/commit/38952b7b2965f4f34e58e986fa681dc291c8a76e))

## [5.29.0](https://github.com/phai-run/phai/compare/v5.28.2...v5.29.0) (2026-06-24)


### Features

* **site:** in-page modals for ADRs and source files on /architecture ([#230](https://github.com/phai-run/phai/issues/230)) ([28a2a8a](https://github.com/phai-run/phai/commit/28a2a8a22dddff6050db01992a3b1b004b533aac))
* **site:** refresh landing — web app screenshots + 'Como funciona' section ([#232](https://github.com/phai-run/phai/issues/232)) ([305a579](https://github.com/phai-run/phai/commit/305a57927ac2efe85f5a8415ccf7de448eed70cf))

## [5.28.2](https://github.com/phai-run/phai/compare/v5.28.1...v5.28.2) (2026-06-24)


### Bug Fixes

* **cli:** guard card_open_bill_due_date against malformed month_ref ([#218](https://github.com/phai-run/phai/issues/218)) ([7facb3f](https://github.com/phai-run/phai/commit/7facb3f635866f7f2002592d6446b72420dd395c))
* **invite:** cap Argon2 KDF params from untrusted envelope to prevent memory-bomb ([#224](https://github.com/phai-run/phai/issues/224)) ([a7bb692](https://github.com/phai-run/phai/commit/a7bb69271ff877b0277005d57e6cb408ce4648aa))
* **serve:** set explicit request body size limit on /api router ([#219](https://github.com/phai-run/phai/issues/219)) ([a9ed493](https://github.com/phai-run/phai/commit/a9ed4934d4f154906c026e4884b114c977c994c0))
* **storage:** cap BigQuery maximum_bytes_billed to prevent runaway cost ([#220](https://github.com/phai-run/phai/issues/220)) ([8c7e7d5](https://github.com/phai-run/phai/commit/8c7e7d5fef6b83ebc0297503152fd9fab16bf8b5))
* **web:** unify uncategorized filter + usability audit 2026-06-24 ([#222](https://github.com/phai-run/phai/issues/222)) ([6ce9dbf](https://github.com/phai-run/phai/commit/6ce9dbfb009ee1c46252843b0f8413cb552ea6b1))


### Performance Improvements

* **serve:** bound read cache with moka and make invalidation granular ([#223](https://github.com/phai-run/phai/issues/223)) ([94a4ab8](https://github.com/phai-run/phai/commit/94a4ab8e74d89fb18d06857826733eefc41a1b08))

## [5.28.1](https://github.com/phai-run/phai/compare/v5.28.0...v5.28.1) (2026-06-22)


### Bug Fixes

* **serve:** dedup duplicate forecast creates by idempotency key ([#215](https://github.com/phai-run/phai/issues/215)) ([0acc0bb](https://github.com/phai-run/phai/commit/0acc0bbbd5f14cde31fbf1df21c06465af824dc2))

## [5.28.0](https://github.com/phai-run/phai/compare/v5.27.4...v5.28.0) (2026-06-22)


### Features

* **web:** manual forecast UX (integrate [#210](https://github.com/phai-run/phai/issues/210)) + UI style fixes ([#213](https://github.com/phai-run/phai/issues/213)) ([b14a4db](https://github.com/phai-run/phai/commit/b14a4db014ecfcc0feec642a66dc588a0009a681))

## [5.27.4](https://github.com/phai-run/phai/compare/v5.27.3...v5.27.4) (2026-06-22)


### Bug Fixes

* **web:** cash-balance badge reflects the balance, not the month net ([#211](https://github.com/phai-run/phai/issues/211)) ([0b68b12](https://github.com/phai-run/phai/commit/0b68b12864ff06ea2c7ab5d27b9eb696e4c32749))

## [5.27.3](https://github.com/phai-run/phai/compare/v5.27.2...v5.27.3) (2026-06-20)


### Bug Fixes

* **web:** include checking-typed accounts in per-account balances ([#208](https://github.com/phai-run/phai/issues/208)) ([2140dfc](https://github.com/phai-run/phai/commit/2140dfc57a9132a77e6385af7556c81269d7d14d))

## [5.27.2](https://github.com/phai-run/phai/compare/v5.27.1...v5.27.2) (2026-06-20)


### Bug Fixes

* **cli:** count bank-typed accounts in pulse, sync notify, and reports ([#206](https://github.com/phai-run/phai/issues/206)) ([c24eef4](https://github.com/phai-run/phai/commit/c24eef49bea8e84f4f7bd8cef3898352ae7f3ca1))

## [5.27.1](https://github.com/phai-run/phai/compare/v5.27.0...v5.27.1) (2026-06-20)


### Bug Fixes

* **storage:** count bank-typed accounts in the cash balance ([#204](https://github.com/phai-run/phai/issues/204)) ([12e9feb](https://github.com/phai-run/phai/commit/12e9feb68af94953ebc589cf49575465f23ce329))

## [5.27.0](https://github.com/phai-run/phai/compare/v5.26.0...v5.27.0) (2026-06-20)


### Features

* **serve:** config-driven friendly account labels ([#201](https://github.com/phai-run/phai/issues/201)) ([d553676](https://github.com/phai-run/phai/commit/d5536760ebee1927bbbdf1dce70a3905a6d702a3))

## [5.26.0](https://github.com/phai-run/phai/compare/v5.25.0...v5.26.0) (2026-06-20)


### Features

* **serve:** per-account checking balance under the cash hero ([#199](https://github.com/phai-run/phai/issues/199)) ([0dd6af3](https://github.com/phai-run/phai/commit/0dd6af3984c52b2a530058a255d3be8647cb5c96))

## [5.25.0](https://github.com/phai-run/phai/compare/v5.24.0...v5.25.0) (2026-06-20)


### Features

* **serve:** config-driven locked categories ([#197](https://github.com/phai-run/phai/issues/197)) ([184dc22](https://github.com/phai-run/phai/commit/184dc223578417a0a5d52feb03eada87f66a27ef))

## [5.24.0](https://github.com/phai-run/phai/compare/v5.23.0...v5.24.0) (2026-06-20)


### Features

* **web:** planning keeps the annual chart pinned + column tooltips ([#195](https://github.com/phai-run/phai/issues/195)) ([8945416](https://github.com/phai-run/phai/commit/8945416731b7a1aae99d1d5aadbe89389d0b84f0))

## [5.23.0](https://github.com/phai-run/phai/compare/v5.22.0...v5.23.0) (2026-06-20)


### Features

* **serve:** Pluggy sync button in the web app ([#193](https://github.com/phai-run/phai/issues/193)) ([2b406ab](https://github.com/phai-run/phai/commit/2b406abb4a639e9a587d23128f440f0dcdbec4f8))

## [5.22.0](https://github.com/phai-run/phai/compare/v5.21.0...v5.22.0) (2026-06-20)


### Features

* **web:** per-segment hover on the expenses chart ([#191](https://github.com/phai-run/phai/issues/191)) ([c43aadc](https://github.com/phai-run/phai/commit/c43aadc79db4fc3c2287397411d41ef6cab77963))

## [5.21.0](https://github.com/phai-run/phai/compare/v5.20.0...v5.21.0) (2026-06-20)


### Features

* **web:** floating hover balloon on the cash chart ([#189](https://github.com/phai-run/phai/issues/189)) ([013ce5d](https://github.com/phai-run/phai/commit/013ce5d8731829ccdf36a913a3cc8d566c171fae))

## [5.20.0](https://github.com/phai-run/phai/compare/v5.19.0...v5.20.0) (2026-06-20)


### Features

* **web:** cards as a tab, declutter planning (hide locked), export fix ([#187](https://github.com/phai-run/phai/issues/187)) ([0c7e284](https://github.com/phai-run/phai/commit/0c7e284e7ca38f6ad82d94f1cdcbd968f834f8fd))

## [5.19.0](https://github.com/phai-run/phai/compare/v5.18.1...v5.19.0) (2026-06-20)


### Features

* **web:** show locked categories read-only in planning ([#185](https://github.com/phai-run/phai/issues/185)) ([2b0b1d1](https://github.com/phai-run/phai/commit/2b0b1d147463514227eb32f762911151365de040))

## [5.18.1](https://github.com/phai-run/phai/compare/v5.18.0...v5.18.1) (2026-06-19)


### Bug Fixes

* **web:** hide commitment-tier badge on income rows ([#183](https://github.com/phai-run/phai/issues/183)) ([88bb6d9](https://github.com/phai-run/phai/commit/88bb6d95fb07726bfac4fb911e5e86345b0ff8a9))

## [5.18.0](https://github.com/phai-run/phai/compare/v5.17.0...v5.18.0) (2026-06-19)


### Features

* **web:** commitment-tier UX — badges, edit-preserves-tier, planning excludes locked ([#180](https://github.com/phai-run/phai/issues/180)) ([3d4d42b](https://github.com/phai-run/phai/commit/3d4d42b0f2373332808e3bcd6ce4480876daf1c5))

## [5.17.0](https://github.com/phai-run/phai/compare/v5.16.0...v5.17.0) (2026-06-19)


### Features

* zero-CLI multi-device activation (invite → install → onboard) ([#177](https://github.com/phai-run/phai/issues/177)) ([119eced](https://github.com/phai-run/phai/commit/119ecede92ce027b1a710f0df6d779953492323a))

## [5.16.0](https://github.com/phai-run/phai/compare/v5.15.0...v5.16.0) (2026-06-19)


### Features

* merge duplicate manual transactions ([#173](https://github.com/phai-run/phai/issues/173)) ([b51c3f1](https://github.com/phai-run/phai/commit/b51c3f1a8d331711afd6e4416e120549db589519))
* **web:** show filtered sheet totals ([#175](https://github.com/phai-run/phai/issues/175)) ([140d29f](https://github.com/phai-run/phai/commit/140d29fc815f2a4267e5c9d5688370d80bda7f65))


### Bug Fixes

* **web:** persist commitment tier overrides after reload ([#174](https://github.com/phai-run/phai/issues/174)) ([510f386](https://github.com/phai-run/phai/commit/510f386a82960209026be661d9cc9270f760a474))
* **web:** prevent sheet amount sign wrapping ([#172](https://github.com/phai-run/phai/issues/172)) ([8bb97fc](https://github.com/phai-run/phai/commit/8bb97fceed90eadd3f7d5455dad5e73ada514f5c))

## [5.15.0](https://github.com/phai-run/phai/compare/v5.14.1...v5.15.0) (2026-06-18)


### Features

* complete quality bug batch ([6994d14](https://github.com/phai-run/phai/commit/6994d14a5ac8185a8af9426f9a0718daa42ab4df))


### Bug Fixes

* **web:** reflect edits in the list, floating sync+version badge ([#169](https://github.com/phai-run/phai/issues/169)) ([f7c3f34](https://github.com/phai-run/phai/commit/f7c3f3455cb08f07a558994e34ad6f2e60504cff))

## [5.14.1](https://github.com/phai-run/phai/compare/v5.14.0...v5.14.1) (2026-06-18)


### Bug Fixes

* **update:** require a verified signature on update (ADR-0017) ([#167](https://github.com/phai-run/phai/issues/167)) ([e206019](https://github.com/phai-run/phai/commit/e20601998482e09e68085c5df328d1b26ba6b285))

## [5.14.0](https://github.com/phai-run/phai/compare/v5.13.0...v5.14.0) (2026-06-18)


### Features

* **update:** confirm minisign signature verification on update ([#165](https://github.com/phai-run/phai/issues/165)) ([6ddd6b9](https://github.com/phai-run/phai/commit/6ddd6b907c0e4efe87f4107dd28b6efbf1f9fcea))

## [5.13.0](https://github.com/phai-run/phai/compare/v5.12.0...v5.13.0) (2026-06-18)


### Features

* per-transaction commitment-tier override + English labels (ADR-0032) ([#161](https://github.com/phai-run/phai/issues/161)) ([c947b5e](https://github.com/phai-run/phai/commit/c947b5ed126bb376e17009a17a043b0fa2ebc194))

## [5.12.0](https://github.com/phai-run/phai/compare/v5.11.0...v5.12.0) (2026-06-18)


### Features

* **web:** commitment tiers + planning goal solver (ADR-0030/0031) ([#159](https://github.com/phai-run/phai/issues/159)) ([f9b72a4](https://github.com/phai-run/phai/commit/f9b72a42f8dee19d03c6abe3a7818619284a5320))

## [5.11.0](https://github.com/phai-run/phai/compare/v5.10.0...v5.11.0) (2026-06-15)


### Features

* **web:** warn before closing with unsynced writes + make sync chip clickable ([#157](https://github.com/phai-run/phai/issues/157)) ([d6dee85](https://github.com/phai-run/phai/commit/d6dee85a78332210842af96242d7d4dcfd5e909a))

## [5.10.0](https://github.com/phai-run/phai/compare/v5.9.2...v5.10.0) (2026-06-15)


### Features

* **serve:** phai serve install --system — root LaunchDaemon on port 80 via admin-auth prompt ([#154](https://github.com/phai-run/phai/issues/154)) ([6bef3c4](https://github.com/phai-run/phai/commit/6bef3c45750df17b30105ca830279fb75d9ca165))
* **serve:** pin Phai.app to the Dock on install ([#155](https://github.com/phai-run/phai/issues/155)) ([cb53fb9](https://github.com/phai-run/phai/commit/cb53fb959997fa1b206bde3d49bcac18e5009c6a))
* **web:** branded loading skeletons for cold start ([#153](https://github.com/phai-run/phai/issues/153)) ([25a8cf8](https://github.com/phai-run/phai/commit/25a8cf8eca5a9bb909e1d22d03b610d0b7ffb8cc))


### Performance Improvements

* **serve:** add TTL read cache to the web bridge ([#152](https://github.com/phai-run/phai/issues/152)) ([fa0ef0a](https://github.com/phai-run/phai/commit/fa0ef0a1bf726563ab65e8f3430c2bfed6c1c5e3))

## [5.9.2](https://github.com/phai-run/phai/compare/v5.9.1...v5.9.2) (2026-06-12)


### Bug Fixes

* **serve:** block DNS rebinding + harden serve bridge and BigQuery search ([253f81e](https://github.com/phai-run/phai/commit/253f81e55ff0a95a0d436156fd07625472e460b3))
* **serve:** block DNS rebinding, stop error leaks, cap event batch ([7d11794](https://github.com/phai-run/phai/commit/7d11794bcb3226b8b7d650f25489c408212da42f))
* **storage:** escape LIKE wildcards in BigQuery search ([55fe27c](https://github.com/phai-run/phai/commit/55fe27c6b6fa1e0f21a5d458ad1974a9e9dee9ad))

## [5.9.1](https://github.com/phai-run/phai/compare/v5.9.0...v5.9.1) (2026-06-12)


### Bug Fixes

* tighten financial invariants ([c58bb20](https://github.com/phai-run/phai/commit/c58bb202aed6bfc5039c484d2325c78995be9cd8))

## [5.9.0](https://github.com/phai-run/phai/compare/v5.8.0...v5.9.0) (2026-06-12)


### Features

* **cli:** MCP server — read-only reports over stdio (phai mcp) ([#146](https://github.com/phai-run/phai/issues/146)) ([f3280ef](https://github.com/phai-run/phai/commit/f3280efdf17ff4f2d32aa188bf6577e12d3e2b36))
* **cli:** phai serve install — launchd agent + Phai.app launcher (macOS) ([#147](https://github.com/phai-run/phai/issues/147)) ([0ef4ee2](https://github.com/phai-run/phai/commit/0ef4ee2283fa913176a2e663d2800cf504198f74))
* **web:** English UI ([#142](https://github.com/phai-run/phai/issues/142)) ([798143d](https://github.com/phai-run/phai/commit/798143dec7ea2caaca04a82fa992ceb237a2f1e1))

## [5.8.0](https://github.com/phai-run/phai/compare/v5.7.0...v5.8.0) (2026-06-11)


### Features

* **web:** metas por subcategoria no plano de guerra com envelopes persistidos ([#140](https://github.com/phai-run/phai/issues/140)) ([4a72843](https://github.com/phai-run/phai/commit/4a72843ac258c85e9c82def1a8bd1d4ce190054e))

## [5.7.0](https://github.com/phai-run/phai/compare/v5.6.1...v5.7.0) (2026-06-11)


### Features

* **web:** opaque sticky sheet headers, larger type, category emojis, level-3 treemap tiles ([#138](https://github.com/phai-run/phai/issues/138)) ([98001d5](https://github.com/phai-run/phai/commit/98001d5681f32dc00c4426866c5502e4e0f27dd6))

## [5.6.1](https://github.com/phai-run/phai/compare/v5.6.0...v5.6.1) (2026-06-10)


### Bug Fixes

* **web:** boot fresh livestore store on schema drift and never skip seeding empty tables ([#136](https://github.com/phai-run/phai/issues/136)) ([763bb81](https://github.com/phai-run/phai/commit/763bb812235705aeb4e0cdce7e0f38564a9a0640))

## [5.6.0](https://github.com/phai-run/phai/compare/v5.5.0...v5.6.0) (2026-06-10)


### Features

* **web:** modo planilha, plano de guerra e dedup de parcelas renomeadas ([#134](https://github.com/phai-run/phai/issues/134)) ([8c0ca52](https://github.com/phai-run/phai/commit/8c0ca5208a0468c353938529dcbd82a23865226e))

## [5.5.0](https://github.com/phai-run/phai/compare/v5.4.1...v5.5.0) (2026-06-03)


### Features

* **web:** dashboard UX/UI redesign — decision hero, chart, cards, filters, category overview ([#129](https://github.com/phai-run/phai/issues/129)) ([74e2b82](https://github.com/phai-run/phai/commit/74e2b8209673d472d6a8fbd222891fe83026f4a5))

## [5.4.1](https://github.com/phai-run/phai/compare/v5.4.0...v5.4.1) (2026-06-03)


### Bug Fixes

* **web:** trustworthy + fast + delightful serve dashboard (data, cards, tx, chart, perf) ([#127](https://github.com/phai-run/phai/issues/127)) ([3c7700d](https://github.com/phai-run/phai/commit/3c7700dad9414007fe337b201b0821b688e54f9e))

## [5.4.0](https://github.com/phai-run/phai/compare/v5.3.0...v5.4.0) (2026-06-03)


### Features

* **report:** CSV export ([941436a](https://github.com/phai-run/phai/commit/941436a91e61d931e29fe02977442389a8fd4f14))

## [5.3.0](https://github.com/phai-run/phai/compare/v5.2.1...v5.3.0) (2026-06-02)


### Features

* **cashflow:** cash-flow basis with bill explosion + single deduped view chain ([#122](https://github.com/phai-run/phai/issues/122)) ([e9f3710](https://github.com/phai-run/phai/commit/e9f37109888b62599d3a5dd9ff6414fd321f6b51))

## [5.2.1](https://github.com/phai-run/phai/compare/v5.2.0...v5.2.1) (2026-05-31)


### Bug Fixes

* **ci:** declare web pnpm workspace package ([6fdd05c](https://github.com/phai-run/phai/commit/6fdd05ccf82bc8f4b51caae621e815a4d600ade2))

## [5.2.0](https://github.com/phai-run/phai/compare/v5.1.3...v5.2.0) (2026-05-31)


### Features

* **serve:** paginated transactions API with offset/hasMore + forecast past-month guard ([d7e3f81](https://github.com/phai-run/phai/commit/d7e3f813a3d919a11e7da252dc1048b3f66b6129))
* **web:** hierarchical category grouping, keyboard shortcuts, chart modes, list performance ([b0d5847](https://github.com/phai-run/phai/commit/b0d5847774fb639078477948b3f23aef90401c53))


### Bug Fixes

* **web:** harden serve ux integration ([9a9e35c](https://github.com/phai-run/phai/commit/9a9e35cb7ea2b7411cb7eee8844ce501ea067fc6))

## [5.1.3](https://github.com/phai-run/phai/compare/v5.1.2...v5.1.3) (2026-05-31)


### Bug Fixes

* **serve:** address quality audit findings [#3](https://github.com/phai-run/phai/issues/3)-[#12](https://github.com/phai-run/phai/issues/12) ([8c09905](https://github.com/phai-run/phai/commit/8c09905a06b8b90a0a2b0bc09907e42238bdda94))

## [5.1.2](https://github.com/phai-run/phai/compare/v5.1.1...v5.1.2) (2026-05-31)


### Bug Fixes

* **serve:** propagate serialization error instead of silently defaulting to empty audit diff ([#116](https://github.com/phai-run/phai/issues/116)) ([29c31b6](https://github.com/phai-run/phai/commit/29c31b6c5e88d223c3c9336b885f27b0f4a35495))

## [5.1.1](https://github.com/phai-run/phai/compare/v5.1.0...v5.1.1) (2026-05-30)


### Bug Fixes

* **cli:** show snapshot timestamp in balances and auto-clear local_db_path on BigQuery setup ([294e750](https://github.com/phai-run/phai/commit/294e75042e001d5076f33c22b53556ecbaab8acf))
* **cli:** show snapshot timestamp in balances and auto-clear local_db_path on BigQuery setup ([3f062d8](https://github.com/phai-run/phai/commit/3f062d8561f16ef195e6e8cbc8bee63d8cc1290f))

## [5.1.0](https://github.com/phai-run/phai/compare/v5.0.0...v5.1.0) (2026-05-30)


### Features

* **forecast:** harden template idempotency with natural keys + self-healing dedup ([#111](https://github.com/phai-run/phai/issues/111)) ([a646679](https://github.com/phai-run/phai/commit/a646679f674bbdbd944b47d32e67899d6bb0425c))

## [5.0.0](https://github.com/phai-run/phai/compare/v4.1.0...v5.0.0) (2026-05-30)


### ⚠ BREAKING CHANGES

* BigQuery runtimes that rewired v_transactions_effective to a Google Sheet will lose Drive access — queries against a Sheets-backed external table will fail. Move those overrides into rules / private configuration.

### Features

* remove Google Sheets category-override sync ([3752867](https://github.com/phai-run/phai/commit/37528675f267c3ae59bd6dff01e78486752ba76f))


### Bug Fixes

* **storage:** accept both 'ativo' and 'active' forecast status in upcoming_forecasts ([d354325](https://github.com/phai-run/phai/commit/d3543259400366ed0a88e2a0709f95ca54c509e0))
* **storage:** accept both 'ativo' and 'active' forecast status in upcoming_forecasts ([d354325](https://github.com/phai-run/phai/commit/d3543259400366ed0a88e2a0709f95ca54c509e0))
* **storage:** accept both 'ativo' and 'active' forecast status in upcoming_forecasts ([c4caaab](https://github.com/phai-run/phai/commit/c4caaabd0a013784362d607846714a25cb8e037f)), closes [#109](https://github.com/phai-run/phai/issues/109)

## [4.1.0](https://github.com/phai-run/phai/compare/v4.0.0...v4.1.0) (2026-05-29)


### Features

* migrate on-disk identity to phai (finance-os fallback) ([f57ac4a](https://github.com/phai-run/phai/commit/f57ac4abe6fb8c62977dfddd2970115f76754f8d))
* migrate on-disk identity to phai with finance-os fallback ([0f0ae83](https://github.com/phai-run/phai/commit/0f0ae83f0b7bf0314f21b7e456665c8bb5190ccf))


### Bug Fixes

* address code review on identity migration ([5088b3b](https://github.com/phai-run/phai/commit/5088b3bb3997a855afe8fae564cdc7de3a5ec96c))

## [4.0.0](https://github.com/phai-run/phai/compare/v3.2.4...v4.0.0) (2026-05-29)


### ⚠ BREAKING CHANGES

* **cli:** the CLI binary is renamed from `fin` to `phai` and the crates from finance-core/finance-cli to phai-core/phai-cli. Existing installs must reinstall (curl -fsSL https://raw.githubusercontent.com/phai-run/phai/main/install.sh | bash); the old `fin` command no longer exists.
* the installed binary is now `phai` (was `fin`). Existing users must reinstall; the old `fin` binary is not upgraded in place.

### Features

* **cli:** add phai branding to version and help ([075f704](https://github.com/phai-run/phai/commit/075f704af6c536d06ca6c9f62fd1e45f3624e4e9))
* **site:** publish phai.run landing via GitHub Pages ([1aa1967](https://github.com/phai-run/phai/commit/1aa1967b98c5b6f76cb6f3704821f420c35b0d42))


### Bug Fixes

* **cli:** correct remaining finance-cli/-core brand strings in user-facing output ([9cd4705](https://github.com/phai-run/phai/commit/9cd470517d82a7c2d471856d24f1ff2dd62e25f1))


### Code Refactoring

* rename crates to phai-core/phai-cli and binary to phai ([1fa5f8c](https://github.com/phai-run/phai/commit/1fa5f8cb62b864f182b33ced8999e2dea05b7b77))

## [3.2.4](https://github.com/feliperun/finance-os/compare/v3.2.3...v3.2.4) (2026-05-28)


### Bug Fixes

* **release:** read unprefixed release-please outputs for root package ([#97](https://github.com/feliperun/finance-os/issues/97)) ([17384cc](https://github.com/feliperun/finance-os/commit/17384cc7249bcfb98ca1f118c2a14fb03b93e905))

## [3.2.3](https://github.com/feliperun/finance-os/compare/v3.2.2...v3.2.3) (2026-05-28)


### Bug Fixes

* **release:** collapse workspace to a single version ([#93](https://github.com/feliperun/finance-os/issues/93)) ([633f8c0](https://github.com/feliperun/finance-os/commit/633f8c03b9c014fb9bf60b20cf1b986a40a05ea8))
* **release:** drive crate versions via simple strategy + extra-files ([#95](https://github.com/feliperun/finance-os/issues/95)) ([254ed03](https://github.com/feliperun/finance-os/commit/254ed038b20afabae6a3df0a961162a0130ddeb1))
* **release:** use literal crate versions for release-please rust updater ([#94](https://github.com/feliperun/finance-os/issues/94)) ([e887f77](https://github.com/feliperun/finance-os/commit/e887f771dd94055ce14ed4484c65b60dcb3f2ffb))

## 0.3.0 - 2026-05-12

### Features

- Add BigQuery-only transaction splits with split lines and receipt items.
- Add deterministic CLI commands for split preview, apply, show, and clear.
- Add split candidate and item price reports for assistant workflows.
- Update assistant integration docs for split-aware Finance OS reporting.

### Infrastructure

- Configure Release Please for future Rust workspace releases.
