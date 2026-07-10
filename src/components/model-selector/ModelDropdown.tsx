import React from "react";
import { useTranslation } from "react-i18next";
import { Cloud } from "lucide-react";
import type { ModelInfo } from "@/bindings";
import {
  getTranslatedModelName,
  getTranslatedModelDescription,
} from "../../lib/utils/modelTranslation";
import { useSettings } from "../../hooks/useSettings";

interface ModelDropdownProps {
  models: ModelInfo[];
  currentModelId: string;
  onModelSelect: (modelId: string) => void;
}

const ModelDropdown: React.FC<ModelDropdownProps> = ({
  models,
  currentModelId,
  onModelSelect,
}) => {
  const { t } = useTranslation();
  const { settings } = useSettings();
  const downloadedModels = models.filter((m) => m.is_downloaded);

  // 云端模型即使未配置凭据也保留在列表里(配置入口在 Settings → Models),
  // 但要标出"未配置",否则选中后要到转写失败时才发现不可用。
  const isCloudModelUnconfigured = (model: ModelInfo): boolean => {
    if (model.engine_type === "Doubao") {
      return (settings?.doubao_credentials?.api_key ?? "").trim().length === 0;
    }
    return false;
  };

  const handleModelClick = (modelId: string) => {
    onModelSelect(modelId);
  };

  return (
    <div className="absolute bottom-full start-0 mb-2 w-64 max-h-[60vh] overflow-y-auto bg-background border border-mid-gray/20 rounded-lg shadow-lg py-2 z-50">
      {downloadedModels.length > 0 ? (
        <div>
          {downloadedModels.map((model) => (
            <div
              key={model.id}
              onClick={() => handleModelClick(model.id)}
              onKeyDown={(e) => {
                if (e.key === "Enter" || e.key === " ") {
                  e.preventDefault();
                  handleModelClick(model.id);
                }
              }}
              tabIndex={0}
              role="button"
              className={`w-full px-3 py-2 text-start hover:bg-mid-gray/10 transition-colors cursor-pointer focus:outline-none ${
                currentModelId === model.id
                  ? "bg-logo-primary/10 text-logo-primary"
                  : ""
              }`}
            >
              <div className="flex items-center justify-between">
                <div>
                  <div className="flex items-center gap-1.5 text-sm text-text/80">
                    <span>{getTranslatedModelName(model, t)}</span>
                    {model.is_cloud && (
                      <Cloud
                        className="w-3 h-3 shrink-0 text-logo-primary/70"
                        aria-label={t("modelSelector.capabilities.cloud")}
                      />
                    )}
                    {model.is_custom && (
                      <span className="text-[10px] font-medium text-text/40 uppercase">
                        {t("modelSelector.custom")}
                      </span>
                    )}
                    {isCloudModelUnconfigured(model) && (
                      <span className="shrink-0 rounded bg-amber-500/10 px-1 py-0.5 text-[10px] font-medium text-amber-500">
                        {t("modelSelector.notConfigured")}
                      </span>
                    )}
                  </div>
                  <div className="text-xs text-text/40 italic pe-4">
                    {getTranslatedModelDescription(model, t)}
                  </div>
                </div>
                {currentModelId === model.id && (
                  <div className="text-xs text-logo-primary">
                    {t("modelSelector.active")}
                  </div>
                )}
              </div>
            </div>
          ))}
        </div>
      ) : (
        <div className="px-3 py-2 text-sm text-text/60">
          {t("modelSelector.noModelsAvailable")}
        </div>
      )}
    </div>
  );
};

export default ModelDropdown;
