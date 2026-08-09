# ppstore

`ppstore` — самостоятельный CLI-клиент для Mac App Store. Он умеет искать
приложения, показывать установленные приложения и доступные обновления, а также
отправлять запросы на установку и обновление через системные службы macOS.

## Установка

Из корня репозитория:

```bash
cargo install --locked --path packages/ppstore
```

Или из этой директории:

```bash
cargo install --locked --path .
```

Для установки через unsigned macOS installer package сначала соберите `.pkg`:

```bash
packages/ppstore/scripts/build-pkg.sh
```

Скрипт только создаёт пакет в `packages/ppstore/target/pkg/`. Он не запускает
`installer` и ничего не записывает в `/usr/local`. После проверки пакет можно
установить вручную; его payload содержит `/usr/local/bin/ppstore`. Имя пакета
содержит архитектуру текущего Mac (`arm64` или `x86_64`); это не universal
binary.

Пакет unsigned: сначала проверьте его путь, payload и отсутствие подписи, и
только затем при осознанном согласии запустите установку:

```bash
PKG="packages/ppstore/target/pkg/ppstore-0.1.0-$(uname -m).pkg"
/usr/sbin/pkgutil --payload-files "$PKG"
/usr/sbin/pkgutil --check-signature "$PKG"
/usr/bin/sudo /usr/sbin/installer -pkg "$PKG" -target /
```

## Команды

```bash
ppstore search Xcode --country US --limit 10
ppstore list
ppstore installed                 # alias для list
ppstore outdated
ppstore doctor

# Без --yes это только безопасный план
ppstore install 497799835 640199958
ppstore upgrade
ppstore update 497799835           # alias для upgrade

# Отправить реальные запросы системной службе App Store
ppstore install 497799835 --yes
ppstore install 640199958 --get --yes
ppstore upgrade --yes

# Один JSON-документ в stdout
ppstore -o json list
```

`--country` принимает двухбуквенный код storefront. Если флаг отсутствует,
используется `PPSTORE_COUNTRY`, затем регион из `AppleLocale`, затем `US`.
К `list`, `outdated` и `doctor` можно несколько раз передать
`--app-root /путь/к/Applications`.

Установка и обновление всегда работают в dry-run режиме без `--yes`.
`--no-wait` возвращает управление после ограниченного ожидания подтверждения
очереди. Статус `pending` означает, что запрос уже отправлен, но его результат
не подтверждён локально: перед повтором следует снова выполнить `list` или
`outdated`.

### Машинный JSON-контракт

`ppstore -o json install ...` и `ppstore -o json upgrade ...` (включая alias
`update`) печатают в stdout ровно один объект `MutationReport`. Его стабильная
версия протокола находится в обязательном числовом поле:

```json
{
  "protocol_version": 1,
  "country": "US",
  "operation": "install",
  "apply": false,
  "wait": true,
  "timeout_millis": 3600000,
  "requested_count": 1,
  "results": [
    {
      "adam_id": 497799835,
      "name": "Xcode",
      "bundle_id": "com.apple.dt.Xcode",
      "installed_version": null,
      "target_version": "26.0",
      "status": "planned",
      "downloads_queued": null,
      "message": "would enqueue install"
    }
  ],
  "warnings": [],
  "errors": []
}
```

Для `--get` значение `operation` равно `get`, для обновления — `update`.
Потребителю следует проверять `protocol_version == 1`, принимать новые
неизвестные поля в рамках этой версии и считать ненулевой exit code признаком
ошибки batch-запроса. Сообщение об итоговой ошибке выводится в stderr и не
нарушает единственный JSON-документ в stdout. JSON команд `search`, `list`,
`outdated` и `doctor` не является `MutationReport` и не содержит это поле.

## Ограничения и безопасность

- Требуется macOS и выполненный вход в Mac App Store.
- `--get` разрешён только для бесплатных приложений. Покупку платного
  приложения нужно завершить в интерфейсе App Store.
- Поиск использует публичный Apple Search/Lookup API. Список обновлений является
  списком кандидатов и зависит от актуальности каталога Apple.
- Apple не предоставляет публичный CLI API для установки. Backend установки
  динамически загружает private frameworks `CommerceKit` и `StoreFoundation`,
  проверяет классы и selectors во время запуска и может перестать работать
  после обновления macOS. Команды поиска и инвентаризации продолжают работать,
  если backend установки недоступен.
- `ppstore` не читает и не сохраняет пароль Apple Account.
- Созданный `.pkg` предназначен для локальной/development-установки. Для
  публичного релиза его нужно подписать сертификатом Developer ID Installer,
  notarize и staple; скрипт намеренно не управляет ключами подписи.

---

## English

`ppstore` is a standalone Mac App Store command-line client. It searches the
catalog, inventories receipt-bearing applications, reports potential updates,
and submits install/update requests to macOS App Store services.

Install it from the repository root:

```bash
cargo install --locked --path packages/ppstore
```

Use `scripts/build-pkg.sh` to create an unsigned installer package under
`target/pkg/`. The script only builds the package; it never runs `installer`.
The package payload installs the binary at `/usr/local/bin/ppstore`. It is built
for the current Mac's architecture (`arm64` or `x86_64`), not as a universal
binary.
Inspect the unsigned package before installing it manually:

```bash
PKG="packages/ppstore/target/pkg/ppstore-0.1.0-$(uname -m).pkg"
/usr/sbin/pkgutil --payload-files "$PKG"
/usr/sbin/pkgutil --check-signature "$PKG"
/usr/bin/sudo /usr/sbin/installer -pkg "$PKG" -target /
```

All install and upgrade commands are dry-run unless `--yes` is supplied. A
`pending` result means the request was submitted but not confirmed locally;
rescan before retrying. The installer backend relies on runtime-checked private
macOS frameworks and may require adaptation after a macOS update. A signed-in
App Store account is required, and paid purchases must be completed in the App
Store UI. `ppstore` never reads or stores Apple Account credentials.
The generated package is a local/development artifact. A public release must
be Developer ID Installer-signed, notarized, and stapled; the build script
intentionally never accesses signing credentials.

### Machine-readable JSON contract

`ppstore -o json install ...` and `ppstore -o json upgrade ...` (including the
`update` alias) emit exactly one `MutationReport` object to stdout. Every such
report contains the numeric field `"protocol_version": 1`. The `operation`
value is `install`, `get`, or `update`; `apply` records whether `--yes` was
supplied. Consumers should require protocol version 1 while tolerating new
unknown fields within that version. A failed batch exits non-zero and writes
its final diagnostic to stderr without corrupting the single JSON document in
stdout. JSON produced by `search`, `list`, `outdated`, and `doctor` is a
different report type and does not carry this mutation protocol field.
