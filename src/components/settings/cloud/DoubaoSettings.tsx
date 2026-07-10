import React, { useState, useEffect, useMemo } from "react";
import { useTranslation } from "react-i18next";
import {
  AudioLines,
  Check,
  CheckCircle2,
  ChevronDown,
  ExternalLink,
  Loader2,
  Sparkles,
} from "lucide-react";
import { openUrl } from "@tauri-apps/plugin-opener";

import { commands } from "@/bindings";
import { Alert } from "../../ui/Alert";
import { Button } from "../../ui/Button";
import { ToggleSwitch } from "../../ui/ToggleSwitch";
import { ApiKeyField } from "../PostProcessingSettingsApi/ApiKeyField";
import { useSettings } from "../../../hooks/useSettings";

const RESOURCE_ID_DURATION = "volc.seedasr.sauc.duration";
const RESOURCE_ID_CONCURRENT = "volc.seedasr.sauc.concurrent";

/** 火山引擎控制台:申请 / 查看 X-Api-Key。 */
const GET_KEY_URL = "https://console.volcengine.com/speech/app";
/** 火山引擎语音技术文档(接入教程)。 */
const TUTORIAL_URL = "https://www.volcengine.com/docs/6561";

type TestState =
  | { status: "idle" }
  | { status: "running" }
  | { status: "success"; logid: string }
  | { status: "error"; message: string };

export interface DoubaoSettingsProps {
  /**
   * 初始是否展开凭据表单。设置页默认折叠(头部作为入口);onboarding 场景传 true,
   * 用户点开云端模型卡片后直接看到 API Key 输入框,少一次点击。
   */
  defaultExpanded?: boolean;
}

/**
 * 火山引擎(豆包)流式语音识别服务商配置面板。
 *
 * 视觉上对齐火山引擎控制台的「服务商设置」风格:品牌头部(图标 + 名称 + 接入教程链接 +
 * 配置状态)、全宽 API Key 输入(附「获取密钥」外链)、可选的模型版本卡片(小时版 / 并发版,
 * 带「推荐 / 当前使用」徽章),以及一次轻量握手的「测试连接」。
 *
 * 鉴权用新版控制台的 `X-Api-Key`(而非旧版 App ID + Access Token),与后端
 * `DoubaoClient` 的握手实现保持一致。在豆包模型卡片下渲染(`ModelsSettings.tsx`),
 * onboarding 的云端模型卡片亦复用(`Onboarding.tsx`)。
 */
export const DoubaoSettings: React.FC<DoubaoSettingsProps> = ({
  defaultExpanded = false,
}) => {
  const { t } = useTranslation();
  const { settings, refreshSettings, getSetting, updateSetting, isUpdating } =
    useSettings();

  const credentials = settings?.doubao_credentials ?? {};
  const apiKey = credentials.api_key ?? "";
  const resourceId = credentials.resource_id ?? RESOURCE_ID_DURATION;
  const isConfigured = apiKey.trim().length > 0;

  const [savingKey, setSavingKey] = useState(false);
  const [savingResource, setSavingResource] = useState(false);
  const [testState, setTestState] = useState<TestState>({ status: "idle" });
  // 默认折叠:头部常驻作为入口,凭据表单按需展开,避免与模型卡片堆成两个紧贴的框
  const [expanded, setExpanded] = useState(defaultExpanded);

  // 当用户切换模型再切回豆包时,重置测试状态
  useEffect(() => {
    setTestState({ status: "idle" });
  }, [apiKey, resourceId]);

  const resourceOptions = useMemo(
    () => [
      {
        value: RESOURCE_ID_DURATION,
        label: t("settings.doubao.resourceId.duration"),
        description: t("settings.doubao.resourceId.durationDesc"),
        recommended: true,
      },
      {
        value: RESOURCE_ID_CONCURRENT,
        label: t("settings.doubao.resourceId.concurrent"),
        description: t("settings.doubao.resourceId.concurrentDesc"),
        recommended: false,
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
    if (next === resourceId || savingResource) return;
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
    <div className="border-t border-mid-gray/20">
      {/* 服务商头部 —— 与上方模型卡片同处一个外层框,作为「配置」折叠开关 */}
      <button
        type="button"
        onClick={() => setExpanded((prev) => !prev)}
        aria-expanded={expanded}
        className="flex w-full items-center gap-3 px-4 py-3 text-left transition-colors hover:bg-mid-gray/10"
      >
        <div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-xl bg-gradient-to-br from-logo-primary to-background-ui text-white shadow-sm">
          <AudioLines className="h-5 w-5" />
        </div>
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <h3 className="truncate text-sm font-semibold">
              {t("settings.doubao.provider")}
            </h3>
            <span
              role="link"
              tabIndex={0}
              onClick={(e) => {
                e.stopPropagation();
                openUrl(TUTORIAL_URL);
              }}
              onKeyDown={(e) => {
                if (e.key === "Enter" || e.key === " ") {
                  e.stopPropagation();
                  e.preventDefault();
                  openUrl(TUTORIAL_URL);
                }
              }}
              className="inline-flex cursor-pointer items-center gap-0.5 text-xs font-medium text-logo-primary hover:underline"
            >
              {t("settings.doubao.tutorial")}
              <ExternalLink className="h-3 w-3" />
            </span>
          </div>
          <p className="truncate text-xs text-text/60">
            {t("settings.doubao.providerDesc")}
          </p>
        </div>
        <div
          className={`flex shrink-0 items-center gap-1.5 rounded-full px-2.5 py-1 text-xs font-medium ${
            isConfigured
              ? "bg-green-500/10 text-green-500"
              : "bg-amber-500/10 text-amber-500"
          }`}
        >
          <span
            className={`h-1.5 w-1.5 rounded-full ${
              isConfigured ? "bg-green-500" : "bg-amber-500"
            }`}
          />
          {isConfigured
            ? t("settings.doubao.configured")
            : t("settings.doubao.unconfigured")}
        </div>
        <ChevronDown
          className={`h-4 w-4 shrink-0 text-text/50 transition-transform ${
            expanded ? "rotate-180" : ""
          }`}
        />
      </button>

      {expanded && (
        <div className="space-y-4 border-t border-mid-gray/20 p-4">
          {/* API Key */}
          <div className="space-y-1.5">
            <div className="flex items-center justify-between">
              <label className="text-sm font-medium">
                {t("settings.doubao.apiKey.title")}
              </label>
              <button
                type="button"
                onClick={() => openUrl(GET_KEY_URL)}
                className="inline-flex items-center gap-0.5 text-xs text-logo-primary hover:underline"
              >
                {t("settings.doubao.getKey")}
                <ExternalLink className="h-3 w-3" />
              </button>
            </div>
            <ApiKeyField
              value={apiKey}
              onBlur={handleApiKeyChange}
              placeholder={t("settings.doubao.apiKey.placeholder")}
              disabled={savingKey}
              className="w-full"
            />
          </div>

          {/* 模型版本 —— 可选卡片,对齐火山引擎控制台「模型」区 */}
          <div className="space-y-2">
            <label className="text-sm font-medium">
              {t("settings.doubao.modelSection")}
            </label>
            <div className="space-y-2">
              {resourceOptions.map((option) => {
                const selected = option.value === resourceId;
                return (
                  <button
                    key={option.value}
                    type="button"
                    onClick={() => handleResourceChange(option.value)}
                    disabled={savingResource}
                    className={`flex w-full items-center gap-3 rounded-lg border p-3 text-left transition-colors disabled:cursor-not-allowed ${
                      selected
                        ? "border-logo-primary bg-logo-primary/10"
                        : "border-mid-gray/30 bg-background/40 hover:border-logo-primary/50"
                    }`}
                  >
                    <div
                      className={`flex h-8 w-8 shrink-0 items-center justify-center rounded-lg ${
                        selected
                          ? "bg-logo-primary/20 text-logo-primary"
                          : "bg-mid-gray/15 text-text/50"
                      }`}
                    >
                      <Sparkles className="h-4 w-4" />
                    </div>
                    <div className="min-w-0 flex-1">
                      <div className="flex items-center gap-2">
                        <span className="truncate text-sm font-semibold">
                          {option.label}
                        </span>
                        {option.recommended && (
                          <span className="shrink-0 rounded bg-logo-primary/20 px-1.5 py-0.5 text-[10px] font-semibold text-logo-primary">
                            {t("settings.doubao.recommended")}
                          </span>
                        )}
                      </div>
                      <p className="truncate text-xs text-text/60">
                        {option.description}
                      </p>
                    </div>
                    {selected && (
                      <span className="flex shrink-0 items-center gap-1 rounded-full bg-green-500/10 px-2 py-0.5 text-[11px] font-medium text-green-500">
                        <Check className="h-3 w-3" />
                        {t("settings.doubao.current")}
                      </span>
                    )}
                  </button>
                );
              })}
            </div>
          </div>

          {/* 逐字上屏 —— 录音时把流式中间结果实时键入输入框 */}
          <div className="border-t border-mid-gray/20 pt-3">
            <ToggleSwitch
              checked={getSetting("streaming_paste") ?? false}
              onChange={(enabled) => updateSetting("streaming_paste", enabled)}
              isUpdating={isUpdating("streaming_paste")}
              label={t("settings.doubao.streamingPaste.label")}
              description={t("settings.doubao.streamingPaste.description")}
              descriptionMode="tooltip"
            />
          </div>

          {/* 测试连接 */}
          <div className="flex flex-wrap items-center gap-3 border-t border-mid-gray/20 pt-3">
            <Button
              onClick={handleTestConnection}
              disabled={!apiKey || testState.status === "running"}
              variant="primary-soft"
              size="md"
            >
              {testState.status === "running" ? (
                <span className="flex items-center">
                  <Loader2 className="mr-1.5 h-3.5 w-3.5 animate-spin" />
                  {t("settings.doubao.testConnection.running")}
                </span>
              ) : (
                <span className="flex items-center">
                  <CheckCircle2 className="mr-1.5 h-3.5 w-3.5" />
                  {t("settings.doubao.testConnection.button")}
                </span>
              )}
            </Button>
            <p className="flex-1 text-xs text-text/50">
              {t("settings.doubao.testConnection.description")}
            </p>
          </div>

          {testState.status === "success" && (
            <Alert variant="success">
              {testState.logid
                ? t("settings.doubao.testConnection.successWithLogid", {
                    logid: testState.logid,
                  })
                : t("settings.doubao.testConnection.success")}
            </Alert>
          )}
          {testState.status === "error" && (
            <Alert variant="error">
              {t("settings.doubao.testConnection.failed", {
                error: testState.message,
              })}
            </Alert>
          )}
        </div>
      )}
    </div>
  );
};

export default DoubaoSettings;
