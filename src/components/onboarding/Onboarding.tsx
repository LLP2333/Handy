import React, { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import type { ModelInfo } from "@/bindings";
import type { ModelCardStatus } from "./ModelCard";
import ModelCard from "./ModelCard";
import HandyTextLogo from "../icons/HandyTextLogo";
import { useModelStore } from "../../stores/modelStore";
import { useSettings } from "../../hooks/useSettings";
import { DoubaoSettings } from "../settings/cloud/DoubaoSettings";
import { Button } from "../ui/Button";

interface OnboardingProps {
  onModelSelected: () => void;
}

const Onboarding: React.FC<OnboardingProps> = ({ onModelSelected }) => {
  const { t } = useTranslation();
  const {
    models,
    downloadModel,
    selectModel,
    downloadingModels,
    verifyingModels,
    extractingModels,
    downloadProgress,
    downloadStats,
  } = useModelStore();
  const { settings } = useSettings();
  const [selectedModelId, setSelectedModelId] = useState<string | null>(null);
  const [expandedCloudModelId, setExpandedCloudModelId] = useState<
    string | null
  >(null);
  const [selectingCloudModel, setSelectingCloudModel] = useState(false);

  const isDownloading = selectedModelId !== null;

  // 云端模型 is_downloaded 恒为 true,不能进"待下载"列表;单独分区展示。
  const localModels = models.filter((m: ModelInfo) => !m.is_cloud);
  const cloudModels = models.filter((m: ModelInfo) => m.is_cloud);

  // 云端模型是否已配置凭据(豆包:api_key 非空)。未配置时禁用「使用此模型」。
  const isCloudModelConfigured = (model: ModelInfo): boolean => {
    if (model.engine_type === "Doubao") {
      return (settings?.doubao_credentials?.api_key ?? "").trim().length > 0;
    }
    return false;
  };

  // Watch for the selected model to finish downloading + verifying + extracting
  useEffect(() => {
    if (!selectedModelId) return;

    const model = models.find((m) => m.id === selectedModelId);
    const stillDownloading = selectedModelId in downloadingModels;
    const stillVerifying = selectedModelId in verifyingModels;
    const stillExtracting = selectedModelId in extractingModels;

    if (
      model?.is_downloaded &&
      !stillDownloading &&
      !stillVerifying &&
      !stillExtracting
    ) {
      // Model is ready — select it and transition
      selectModel(selectedModelId).then((success) => {
        if (success) {
          onModelSelected();
        } else {
          toast.error(t("onboarding.errors.selectModel"));
          setSelectedModelId(null);
        }
      });
    }
  }, [
    selectedModelId,
    models,
    downloadingModels,
    verifyingModels,
    extractingModels,
    selectModel,
    onModelSelected,
  ]);

  const handleDownloadModel = async (modelId: string) => {
    setSelectedModelId(modelId);

    // Error toast is handled centrally by the model-download-failed event listener
    // in modelStore — no toast here to avoid duplicates.
    const success = await downloadModel(modelId);
    if (!success) {
      setSelectedModelId(null);
    }
  };

  // 云端模型不下载:配置好凭据后直接选用并完成 onboarding。
  const handleUseCloudModel = async (modelId: string) => {
    setSelectingCloudModel(true);
    try {
      const success = await selectModel(modelId);
      if (success) {
        onModelSelected();
      } else {
        toast.error(t("onboarding.errors.selectModel"));
      }
    } finally {
      setSelectingCloudModel(false);
    }
  };

  const getModelStatus = (modelId: string): ModelCardStatus => {
    if (modelId in extractingModels) return "extracting";
    if (modelId in verifyingModels) return "verifying";
    if (modelId in downloadingModels) return "downloading";
    return "downloadable";
  };

  const getModelDownloadProgress = (modelId: string): number | undefined => {
    return downloadProgress[modelId]?.percentage;
  };

  const getModelDownloadSpeed = (modelId: string): number | undefined => {
    return downloadStats[modelId]?.speed;
  };

  return (
    <div className="h-screen w-screen flex flex-col p-6 gap-4 inset-0">
      <div className="flex flex-col items-center gap-2 shrink-0">
        <HandyTextLogo width={200} />
        <p className="text-text/70 max-w-md font-medium mx-auto">
          {t("onboarding.subtitle")}
        </p>
      </div>

      <div className="max-w-[600px] w-full mx-auto text-center flex-1 flex flex-col min-h-0 overflow-y-auto">
        <div className="flex flex-col gap-4 pb-6">
          {localModels
            .filter((m: ModelInfo) => !m.is_downloaded)
            .filter((model: ModelInfo) => model.is_recommended)
            .map((model: ModelInfo) => (
              <ModelCard
                key={model.id}
                model={model}
                variant="featured"
                status={getModelStatus(model.id)}
                disabled={isDownloading}
                onSelect={handleDownloadModel}
                onDownload={handleDownloadModel}
                downloadProgress={getModelDownloadProgress(model.id)}
                downloadSpeed={getModelDownloadSpeed(model.id)}
              />
            ))}

          {localModels
            .filter((m: ModelInfo) => !m.is_downloaded)
            .filter((model: ModelInfo) => !model.is_recommended)
            .sort(
              (a: ModelInfo, b: ModelInfo) =>
                Number(a.size_mb) - Number(b.size_mb),
            )
            .map((model: ModelInfo) => (
              <ModelCard
                key={model.id}
                model={model}
                status={getModelStatus(model.id)}
                disabled={isDownloading}
                onSelect={handleDownloadModel}
                onDownload={handleDownloadModel}
                downloadProgress={getModelDownloadProgress(model.id)}
                downloadSpeed={getModelDownloadSpeed(model.id)}
              />
            ))}

          {cloudModels.length > 0 && (
            <>
              <p className="text-sm font-medium text-text/50 mt-2">
                {t("onboarding.cloud.sectionTitle")}
              </p>
              {cloudModels.map((model: ModelInfo) => {
                const expanded = expandedCloudModelId === model.id;
                const configured = isCloudModelConfigured(model);
                return (
                  <div
                    key={model.id}
                    className={`overflow-hidden rounded-xl border-2 transition-colors ${
                      expanded ? "border-logo-primary/50" : "border-mid-gray/20"
                    }`}
                  >
                    <ModelCard
                      model={model}
                      bare
                      status="available"
                      disabled={isDownloading}
                      onSelect={() =>
                        setExpandedCloudModelId(expanded ? null : model.id)
                      }
                    />
                    {expanded && (
                      <>
                        <DoubaoSettings defaultExpanded />
                        <div className="flex flex-col items-stretch gap-1.5 border-t border-mid-gray/20 p-4">
                          <Button
                            variant="primary"
                            size="md"
                            disabled={!configured || selectingCloudModel}
                            onClick={() => handleUseCloudModel(model.id)}
                          >
                            {t("onboarding.cloud.useModel")}
                          </Button>
                          {!configured && (
                            <p className="text-xs text-text/50">
                              {t("onboarding.cloud.needsCredentials")}
                            </p>
                          )}
                        </div>
                      </>
                    )}
                  </div>
                );
              })}
            </>
          )}
        </div>
      </div>
    </div>
  );
};

export default Onboarding;
