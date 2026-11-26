/*
 * 合成の例: 入力を読む・送る・設定を NVS から読む本体は利用者のファームウェアが持つ。
 * ここは CI のビルドの形を示すための最小の app_main だけ。
 * - 設定の項目は config-schema.toml（追記だけ）。NVS に無い項目は default を使う。
 * - アナログの換算はしない（生の値を送る。係数は中央の設定のツール）。
 * - OTA は 2 つのアプリのパーティションとロールバック（設定のツールが配る。GitStation は関わらない）。
 */
#include "esp_app_desc.h"
#include "esp_log.h"

static const char *TAG = "edge-fw";

void app_main(void)
{
    const esp_app_desc_t *app = esp_app_get_description();
    ESP_LOGI(TAG, "firmware %s (%s)", app->version, app->project_name);
}
