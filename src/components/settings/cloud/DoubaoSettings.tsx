import React, { useState, useEffect, useMemo } from "react";
import { useTranslation } from "react-i18next";
import { Cloud, Loader2 } from "lucide-react";

import { commands } from "@/bindings";
import { Alert } from "../../ui/Alert";
import { Button } from "../../ui/Button";
import { Dropdown, SettingContainer } from "@/components/ui";
import { ApiKeyField } from "../PostProcessingSettingsApi/ApiKeyField";
import { useSettings } from "../../../hooks/useSettings";

const RESOURCE_ID_DURATION = "volc.seedasr.sauc.duration";
const RESOURCE_ID_CONCURRENT = "volc.seedasr.sauc.concurrent";

type TestState =
  | { status: "idle" }
  | { status: "running" }
  | { status: "success"; logid: string }
  | { status: "error"; message: string };

/**
 * 豆包(火山引擎)流式语音识别凭据设置面板。
 *
 * 包含 API Key 输入(`password`)、资源 ID 选择(2.0 小时版/2.0 并发版)、
 * 「测试连接」按钮(会通过 `commands.testDoubaoConnection` 做一次轻量握手验证)。
 *
 * 仅在用户当前选中模型为 `engine_type === "Doubao"` 时挂载,见 `ModelsSettings.tsx`。
 */
export const DoubaoSettings: React.FC = () => {
  const { t } = useTranslation();
  const { settings, refreshSettings } = useSettings();

  const credentials = settings?.doubao_credentials ?? {};
  const apiKey = credentials.api_key ?? "";
  const resourceId = credentials.resource_id ?? RESOURCE_ID_DURATION;

  const [savingKey, setSavingKey] = useState(false);
  const [savingResource, setSavingResource] = useState(false);
  const [testState, setTestState] = useState<TestState>({ status: "idle" });

  // 当用户切换模型再切回豆包时,重置测试状态
  useEffect(() => {
    setTestState({ status: "idle" });
  }, [apiKey, resourceId]);

  const resourceOptions = useMemo(
    () => [
      {
        value: RESOURCE_ID_DURATION,
        label: t("settings.doubao.resourceId.duration"),
      },
      {
        value: RESOURCE_ID_CONCURRENT,
        label: t("settings.doubao.resourceId.concurrent"),
      },
    ],
    [t],
  );

  const handleApiKeyChange = async (next: string) => {
    if (next === apiKey) return;
    setSavingKey(true);
    try {
      const result = await commands.changeDoubaoCredentialSetting(
        "api_key",
        next,
      );
      if (result.status === "error") {
        console.error("Failed to save Doubao API key:", result.error);
      }
      await refreshSettings();
    } finally {
      setSavingKey(false);
    }
  };

  const handleResourceChange = async (next: string) => {
    if (next === resourceId) return;
    setSavingResource(true);
    try {
      const result = await commands.changeDoubaoCredentialSetting(
        "resource_id",
        next,
      );
      if (result.status === "error") {
        console.error("Failed to save Doubao resource id:", result.error);
      }
      await refreshSettings();
    } finally {
      setSavingResource(false);
    }
  };

  const handleTestConnection = async () => {
    setTestState({ status: "running" });
    const result = await commands.testDoubaoConnection();
    if (result.status === "ok") {
      setTestState({ status: "success", logid: result.data });
    } else {
      setTestState({ status: "error", message: result.error });
    }
  };

  return (
    <div className="rounded-lg border border-mid-gray/30 bg-mid-gray/5 p-4 space-y-3">
      <div className="flex items-center gap-2">
        <Cloud className="w-4 h-4 text-logo-primary" />
        <h3 className="text-sm font-semibold">{t("settings.doubao.title")}</h3>
      </div>
      <p className="text-xs text-text/60 -mt-1">
        {t("settings.doubao.description")}
      </p>

      <SettingContainer
        title={t("settings.doubao.apiKey.title")}
        description={t("settings.doubao.apiKey.description")}
        descriptionMode="tooltip"
        layout="horizontal"
        grouped
      >
        <div className="flex items-center gap-2">
          <ApiKeyField
            value={apiKey}
            onBlur={handleApiKeyChange}
            placeholder={t("settings.doubao.apiKey.placeholder")}
            disabled={savingKey}
            className="min-w-[320px]"
          />
        </div>
      </SettingContainer>

      <SettingContainer
        title={t("settings.doubao.resourceId.title")}
        description={t("settings.doubao.resourceId.description")}
        descriptionMode="tooltip"
        layout="horizontal"
        grouped
      >
        <Dropdown
          selectedValue={resourceId}
          options={resourceOptions}
          onSelect={handleResourceChange}
          disabled={savingResource}
        />
      </SettingContainer>

      <SettingContainer
        title={t("settings.doubao.testConnection.title")}
        description={t("settings.doubao.testConnection.description")}
        descriptionMode="tooltip"
        layout="horizontal"
        grouped
      >
        <Button
          onClick={handleTestConnection}
          disabled={!apiKey || testState.status === "running"}
          variant="secondary"
          size="sm"
        >
          {testState.status === "running" ? (
            <>
              <Loader2 className="w-3.5 h-3.5 mr-1.5 animate-spin" />
              {t("settings.doubao.testConnection.running")}
            </>
          ) : (
            t("settings.doubao.testConnection.button")
          )}
        </Button>
      </SettingContainer>

      {testState.status === "success" && (
        <Alert variant="success" contained>
          {testState.logid
            ? t("settings.doubao.testConnection.successWithLogid", {
                logid: testState.logid,
              })
            : t("settings.doubao.testConnection.success")}
        </Alert>
      )}
      {testState.status === "error" && (
        <Alert variant="error" contained>
          {t("settings.doubao.testConnection.failed", {
            error: testState.message,
          })}
        </Alert>
      )}
    </div>
  );
};

export default DoubaoSettings;
