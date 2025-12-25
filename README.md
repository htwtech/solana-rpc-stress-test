# Solana RPC Stress Test

Высокопроизводительный инструмент для стресс-тестирования Solana RPC endpoints, написанный на Rust. Предназначен для создания максимальной нагрузки на RPC-ноды с минимальным влиянием на производительность самой машины, выполняющей тест.

## Описание

Этот инструмент позволяет:
- Создавать высокую нагрузку на Solana RPC endpoints
- Тестировать несколько методов одновременно с разными параметрами
- Измерять латентность и собирать детальную статистику по ошибкам
- Выполнять предварительный ping-тест для оценки базовой сетевой задержки
- Использовать конфигурационные файлы для сложных сценариев тестирования

## Архитектура и технические детали

### Производительность

Инструмент оптимизирован для максимальной производительности:

- **Lock-free структуры данных**: Использует `AtomicU64` для счетчиков и `SegQueue` (lock-free очередь) для сбора времен ответов
- **Минимальный I/O**: Весь вывод происходит только в конце теста, во время работы воркеров нет записи на диск
- **Асинхронность**: Использует Tokio для параллельного выполнения запросов
- **Оптимизированная сборка**: Release сборка с LTO (Link Time Optimization), opt-level 3 и panic=abort

### Структура данных

- **Stats**: Централизованная структура для сбора статистики, использует Arc для безопасного разделения между потоками
- **HTTP ошибки**: Хранятся в HashMap с ключами вида "код ошибки + описание" (например, "429 Too Many Requests")
- **Времена ответов**: Собираются в lock-free очередь SegQueue для последующего расчета статистики

## Установка

### Требования

- Rust 1.70+ (устанавливается автоматически через rustup)
- OpenSSL dev библиотеки (libssl-dev на Debian/Ubuntu)

### Установка зависимостей

```bash
# Debian/Ubuntu
sudo apt-get update
sudo apt-get install -y libssl-dev pkg-config

# Fedora/RHEL
sudo dnf install openssl-devel pkg-config
```

### Сборка проекта

```bash
# Установка Rust (если еще не установлен)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# Сборка release версии
cargo build --release

# Исполняемый файл будет в target/release/solana-rpc-stress-test
```

## Использование

### Базовое использование

```bash
# Запуск с параметрами по умолчанию (1 воркер, метод getHealth, 60 секунд)
./target/release/solana-rpc-stress-test

# Запуск с параметрами командной строки
./target/release/solana-rpc-stress-test \
  --workers 50 \
  --method getHealth \
  --timeout-ms 50 \
  --url https://api.mainnet-beta.solana.com \
  --duration 120
```

### Использование конфигурационного файла

```bash
# Запуск с конфигом
./target/release/solana-rpc-stress-test -c config.toml

# С дополнительными опциями
./target/release/solana-rpc-stress-test -c config.toml -v -p
```

## Параметры командной строки

### Основные параметры

- `--workers` / `-w`: Количество параллельных воркеров (по умолчанию: 1)
  - Каждый воркер работает в отдельном async-задаче
  - Рекомендуется: 10-200 в зависимости от мощности RPC-ноды

- `--method` / `-m`: RPC метод для запроса (по умолчанию: "getHealth")
  - Примеры: getHealth, getSlot, getVersion, getBlock, simulateTransaction
  - При использовании конфига этот параметр игнорируется

- `--timeout-ms` / `-t`: Таймаут между запросами каждого воркера в миллисекундах (по умолчанию: 1)
  - Минимальное значение: 1 мс
  - Чем меньше значение, тем выше нагрузка

- `--url` / `-u`: URL Solana RPC endpoint (по умолчанию: "https://api.mainnet-beta.solana.com")
  - Поддерживаются HTTP и HTTPS
  - Формат: http://host:port или https://host:port

- `--duration` / `-d`: Продолжительность теста в секундах (по умолчанию: 60)
  - 0 = бесконечный тест (до ручной остановки Ctrl+C)
  - Рекомендуется: 60-300 секунд для стабильных результатов

- `--http-timeout`: HTTP таймаут в секундах (по умолчанию: 30)
  - Таймаут для каждого HTTP запроса
  - При превышении запрос считается таймаутом

### Дополнительные опции

- `--debug` / `-v`: Режим отладки
  - Выводит все ответы RPC на консоль в реальном времени
  - Полезно для проверки корректности запросов
  - **Внимание**: Может снизить производительность из-за I/O операций

- `--ping` / `-p`: Предварительный ping-тест
  - Выполняет 10 ICMP пакетов к хосту RPC endpoint
  - Показывает минимальную, максимальную и среднюю латентность
  - Помогает оценить базовую сетевую задержку
  - Требует наличия команды `ping` в системе

- `--config` / `-c`: Путь к конфигурационному файлу
  - Если указан, параметры берутся из конфига
  - Параметры командной строки используются как fallback для не указанных в конфиге

## Конфигурационный файл

Конфигурационный файл использует формат TOML и позволяет:
- Настраивать несколько методов одновременно
- Указывать разные параметры для каждого метода
- Задавать количество воркеров для каждого метода отдельно

### Формат конфига

```toml
# Общие настройки (опционально, можно переопределить через аргументы командной строки)
url = "https://api.mainnet-beta.solana.com"
timeout_ms = 10
duration = 300
http_timeout = 60

# Список методов для тестирования
[[methods]]
method = "getHealth"
params = []
workers = 10

[[methods]]
method = "getSlot"
params = []
workers = 5

[[methods]]
method = "getBlock"
params = [
  388982336,
  {
    commitment = "finalized",
    encoding = "json",
    transactionDetails = "full",
    maxSupportedTransactionVersion = 0,
    rewards = false
  }
]
workers = 20

[[methods]]
method = "simulateTransaction"
params = [
  "BASE64_ENCODED_TRANSACTION_HERE",
  {
    sigVerify = true,
    encoding = "base64"
  }
]
workers = 50
```

### Параметры конфига

**Общие параметры:**
- `url`: URL RPC endpoint (опционально)
- `timeout_ms`: Таймаут между запросами в миллисекундах (опционально)
- `duration`: Продолжительность теста в секундах (опционально)
- `http_timeout`: HTTP таймаут в секундах (опционально)

**Параметры метода:**
- `method`: Название RPC метода (обязательно)
- `params`: Массив параметров для метода (опционально, по умолчанию пустой массив)
  - Может содержать строки, числа, булевы значения, объекты
  - Для сложных методов (getBlock, simulateTransaction) передаются объекты с опциями
- `workers`: Количество воркеров для этого метода (обязательно)

### Примеры конфигов

**Простой конфиг для базового тестирования:**
```toml
url = "https://api.mainnet-beta.solana.com"
timeout_ms = 1
duration = 60

[[methods]]
method = "getHealth"
params = []
workers = 10
```

**Конфиг для тяжелых методов:**
```toml
url = "https://api.mainnet-beta.solana.com"
timeout_ms = 10
duration = 300
http_timeout = 60

[[methods]]
method = "getRecentPrioritizationFees"
params = []
workers = 20

[[methods]]
method = "getBlock"
params = [388982336, { commitment = "finalized", encoding = "json", transactionDetails = "full", maxSupportedTransactionVersion = 0, rewards = false }]
workers = 100

[[methods]]
method = "simulateTransaction"
params = [
  "AexGySTobarM81MSDjGqPWmLOTmUILJe63xV652hTkDVprgstc/J0sJPQj9eUmni1nCAKPZhgyJUu2WYSg6h+AmAAQAEDGQZPgRMpwv/rCDYd26qaB5VeRcblaT4L69BX/reRfvDCgUkQaFYTleMcAX2p74eBXQZd1dwDyQZAPJfSv2KGc5fhkxp8pIzBK5iqmN41NYLWy9LoGNoODfQgwz+9ZGnzHhSHLF5zruFibVWotXslNJJhoL9+bsq9a1k5JHMQVPauYWuPLoIHj1rtPdLNX3+MqAowL1C3mfnKmBZ/gHZOU3i+HfoPa7GiI9Dq7WuezEbt4w0lgiX/OWnuCwW4g/xl+wzLzzOv1RDutSRJ3yD5zNhIDZ1D8mAZfIoTa1vIR+Y8Yfsh9H3Rcs6AzhKJqae2gyi0aoPQeQkFjd+kf9bXTEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAMGRm/lIRcy/+ytunLDm+e8jOW7xfcSayxDmzpAAAAABHnVW/IxwG7udMVuzmgVB/2xst6j9I5RArHNola8E48K8cNDIYjKOmNRNaE6GJUazr0p5qwu7Tj5Hm3Rsjs8V1AqCsfV6blYjo3oywkKqcTrUhDSQhrBc2dzRKxd4oJHBAkACQMRJwAAAAAAAAgCAAcMAgAAAAEAAAAAAAAACh0TBQACBAEGFhAKChUKDwUMFBcNDgEEEBYTExESCyXBIJszQdacgQABAAAAXwFkAAEWKGvuAAAAAE0yK5EHAAAAAAAACAIAAwwCAAAAQA0DAAAAAAAB1ZuEWF9vDQJ8xEjOJ4CEuZ0VjhuJ+uREzISFmwaXO1oDMzY3CTIIBQYCNAsJNQ==",
  { sigVerify = true, encoding = "base64" }
]
workers = 50
```

**Конфиг с кастомным методом getLatestBlock:**
```toml
url = "https://api.mainnet-beta.solana.com"
timeout_ms = 10
duration = 300
http_timeout = 60

[[methods]]
method = "getLatestBlock"
params = [
  {
    commitment = "finalized",
    encoding = "json",
    transactionDetails = "full",
    maxSupportedTransactionVersion = 0,
    rewards = false
  }
]
workers = 100
```

**Примечание**: Метод `getLatestBlock` автоматически получает актуальный слот перед каждым запросом `getBlock`, создавая двойную нагрузку (getSlot + getBlock) и всегда запрашивая самый свежий блок.

## Статистика и метрики

В конце теста выводится подробная статистика:

### Общая статистика
- **Total requests**: Общее количество отправленных запросов
- **Successful**: Количество успешных запросов и процент успеха

### Детализация ошибок

**HTTP ошибки по типам:**
- Каждая HTTP ошибка выводится отдельно с кодом и описанием
- Пример: "429 Too Many Requests: 150"
- Пример: "500 Internal Server Error: 5"
- Пример: "502 Bad Gateway: 10"

**Другие типы ошибок:**
- **HTTP timeouts**: Количество запросов, превысивших HTTP таймаут
- **JSON parse errors**: Ошибки парсинга JSON ответов
- **Network errors**: Сетевые ошибки (connection refused, DNS и т.д.)
- **RPC errors**: Ошибки в JSON-RPC ответах (когда метод возвращает error в поле error)

### Латентность

- **Average**: Средняя латентность всех успешных запросов (в миллисекундах)
- **Minimum**: Минимальная латентность
- **Maximum**: Максимальная латентность

## Популярные RPC методы

### Легкие методы (для базовой нагрузки)
- `getHealth` - проверка здоровья ноды
- `getSlot` - получение текущего слота
- `getVersion` - получение версии Solana
- `getBlockHeight` - получение высоты блока

### Средние методы
- `getBalance` - получение баланса аккаунта (требует параметр: адрес)
- `getAccountInfo` - получение информации об аккаунте
- `getTokenAccountBalance` - получение баланса токен-аккаунта

### Тяжелые методы (для максимальной нагрузки)
- `getRecentPrioritizationFees` - анализ до 150 последних блоков
- `getProgramAccounts` - сканирование всех аккаунтов программы
- `getBlock` - получение полного блока с транзакциями
- `getTokenLargestAccounts` - поиск крупнейших аккаунтов токена
- `simulateTransaction` - симуляция транзакции (очень CPU-интенсивно с sigVerify=true)

### Кастомные методы

- `getLatestBlock` - **Специальный метод для получения самого свежего блока**
  - Автоматически получает актуальный номер слота через `getSlot` перед каждым запросом `getBlock`
  - Создает двойную нагрузку: сначала `getSlot`, затем `getBlock` с полученным слотом
  - Всегда запрашивает самый свежий блок из базы данных ноды
  - Параметры: опции для `getBlock` (как в обычном `getBlock`, но без указания слота)
  - Пример использования:
    ```toml
    [[methods]]
    method = "getLatestBlock"
    params = [
      {
        commitment = "finalized",
        encoding = "json",
        transactionDetails = "full",
        maxSupportedTransactionVersion = 0,
        rewards = false
      }
    ]
    workers = 100
    ```
  - Если `params` пустой, используются дефолтные опции для `getBlock`
  - **Важно**: Каждая итерация создает 2 RPC-запроса (getSlot + getBlock), что увеличивает нагрузку

## Примеры использования

### Пример 1: Базовый тест одного метода

```bash
./target/release/solana-rpc-stress-test \
  --workers 50 \
  --method getHealth \
  --timeout-ms 10 \
  --duration 120
```

### Пример 2: Тест с ping и debug режимом

```bash
./target/release/solana-rpc-stress-test \
  --url https://api.mainnet-beta.solana.com \
  --workers 100 \
  --method getSlot \
  --timeout-ms 5 \
  --duration 300 \
  --ping \
  --debug
```

### Пример 3: Использование конфига

```bash
# Создайте config.toml с нужными методами
./target/release/solana-rpc-stress-test -c config.toml

# С ping тестом
./target/release/solana-rpc-stress-test -c config.toml -p
```

### Пример 4: Получение транзакции для simulateTransaction

```bash
# 1. Получите подпись транзакции (например, из explorer)
SIGNATURE="5izHHnE3JQe6qZP23EhiR9DNm6ddCjP54Yhd3k58Y3cMiMNT5RQFZwLreMGsA6548rGUHwzNo2oUsYyS4v6yKbqr"

# 2. Получите транзакцию в base64
curl -X POST https://api.mainnet-beta.solana.com \
  -H "Content-Type: application/json" \
  -d "{
    \"jsonrpc\": \"2.0\",
    \"id\": 1,
    \"method\": \"getTransaction\",
    \"params\": [
      \"$SIGNATURE\",
      {
        \"encoding\": \"base64\",
        \"maxSupportedTransactionVersion\": 0
      }
    ]
  }" | jq -r '.result.transaction[0]'

# 3. Скопируйте полученную base64 строку в config.toml
```

## Технические детали реализации

### Сбор статистики

Статистика собирается с использованием lock-free структур:
- `AtomicU64` для счетчиков (total, successful, errors)
- `SegQueue` для времен ответов (lock-free очередь)
- `HashMap` с `Mutex` для HTTP ошибок (используется редко, только при ошибках)

### Обработка ошибок

Каждый тип ошибки обрабатывается отдельно:
1. **HTTP 4xx/5xx**: Извлекается код статуса и описание, создается ключ "код описание"
2. **HTTP timeout**: Определяется через `reqwest::Error::is_timeout()`
3. **JSON parse errors**: Ошибки десериализации JSON ответа
4. **Network errors**: Все остальные сетевые ошибки
5. **RPC errors**: Ошибки в поле `error` JSON-RPC ответа

### Уникальные ID запросов

Каждый воркер использует уникальный диапазон ID:
- `worker_id * 1_000_000 + request_id`
- Это гарантирует отсутствие конфликтов ID между воркерами

### Кастомный метод getLatestBlock

Метод `getLatestBlock` реализован как специальная обработка в функции `worker`:
1. При обнаружении метода `getLatestBlock` воркер сначала вызывает `getSlot` для получения актуального номера слота
2. Затем формируются параметры для `getBlock` с полученным слотом
3. Выполняется запрос `getBlock` с актуальным слотом
4. Если `getSlot` не удался, запрос считается ошибкой и пропускается
5. Время ответа включает оба запроса (getSlot + getBlock)
6. Это создает двойную нагрузку на RPC-ноду, но гарантирует получение самого свежего блока

## Рекомендации по использованию

### Для тестирования производительности RPC-ноды

1. Начните с легких методов и небольшого количества воркеров
2. Постепенно увеличивайте нагрузку
3. Мониторьте статистику ошибок
4. Используйте ping тест для оценки базовой латентности

### Для максимальной нагрузки

1. Используйте тяжелые методы: `getBlock`, `simulateTransaction`, `getProgramAccounts`
2. Для `simulateTransaction` используйте `sigVerify = true` (самая CPU-интенсивная опция)
3. Используйте кастомный метод `getLatestBlock` для создания двойной нагрузки (getSlot + getBlock)
4. Увеличьте количество воркеров до 100-200
5. Уменьшите `timeout_ms` до 1-5 мс
6. Используйте несколько методов одновременно через конфиг

### Важные замечания

- **Не используйте `sigVerify` и `replaceRecentBlockhash` вместе** - это вызовет ошибку
- Для `simulateTransaction` транзакция должна быть в формате base64 (не base58)
- При использовании устаревших транзакций используйте `replaceRecentBlockhash = true`
- Для максимальной нагрузки используйте только `sigVerify = true`

## Устранение проблем

### Ошибка "invalid base58 encoding"
- Убедитесь, что для `simulateTransaction` передаете base64 транзакцию, а не адрес или подпись
- Получите транзакцию через `getTransaction` с `encoding: "base64"`

### Ошибка "sigVerify may not be used with replaceRecentBlockhash"
- Используйте только один из этих параметров
- Для максимальной нагрузки используйте `sigVerify = true`

### Высокий процент ошибок
- Уменьшите количество воркеров
- Увеличьте `timeout_ms` между запросами
- Увеличьте `http_timeout`
- Проверьте доступность RPC endpoint

## Лицензия

Проект создан для тестирования и оценки производительности Solana RPC endpoints.
