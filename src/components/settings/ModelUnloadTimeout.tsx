import React, { useMemo } from "react";
import { useTranslation } from "react-i18next";
import { useSettings } from "../../hooks/useSettings";
import { useModelStore } from "../../stores/modelStore";
import { commands, type ModelUnloadTimeout } from "@/bindings";
import { Dropdown } from "../ui/Dropdown";
import { SettingContainer } from "../ui/SettingContainer";

interface ModelUnloadTimeoutProps {
  descriptionMode?: "tooltip" | "inline";
  grouped?: boolean;
}

export const ModelUnloadTimeoutSetting: React.FC<ModelUnloadTimeoutProps> = ({
  descriptionMode = "inline",
  grouped = false,
}) => {
  const { t } = useTranslation();
  const { settings, getSetting, updateSetting } = useSettings();
  const { currentModel, models } = useModelStore();

  // 云端模型没有本地引擎驻留内存,此设置对它不生效(只影响之后切回的本地模型)。
  // 激活云端模型时换用说明文案,避免"释放 GPU/CPU 内存"的描述造成误导。
  const isCloudModelActive =
    models.find((m) => m.id === currentModel)?.is_cloud ?? false;

  const timeoutOptions = [
    {
      value: "never" as ModelUnloadTimeout,
      label: t("settings.advanced.modelUnload.options.never"),
    },
    {
      value: "immediately" as ModelUnloadTimeout,
      label: t("settings.advanced.modelUnload.options.immediately"),
    },
    {
      value: "min2" as ModelUnloadTimeout,
      label: t("settings.advanced.modelUnload.options.min2"),
    },
    {
      value: "min5" as ModelUnloadTimeout,
      label: t("settings.advanced.modelUnload.options.min5"),
    },
    {
      value: "min10" as ModelUnloadTimeout,
      label: t("settings.advanced.modelUnload.options.min10"),
    },
    {
      value: "min15" as ModelUnloadTimeout,
      label: t("settings.advanced.modelUnload.options.min15"),
    },
    {
      value: "hour1" as ModelUnloadTimeout,
      label: t("settings.advanced.modelUnload.options.hour1"),
    },
  ];

  const debugTimeoutOptions = [
    ...timeoutOptions,
    {
      value: "sec15" as ModelUnloadTimeout,
      label: t("settings.advanced.modelUnload.options.sec15"),
    },
  ];

  const handleChange = async (event: React.ChangeEvent<HTMLSelectElement>) => {
    const newTimeout = event.target.value as ModelUnloadTimeout;

    try {
      await commands.setModelUnloadTimeout(newTimeout);
      updateSetting("model_unload_timeout", newTimeout);
    } catch (error) {
      console.error("Failed to update model unload timeout:", error);
    }
  };

  const currentValue = getSetting("model_unload_timeout") ?? "never";

  const options = useMemo(() => {
    return settings?.debug_mode === true ? debugTimeoutOptions : timeoutOptions;
  }, [settings]);

  return (
    <SettingContainer
      title={t("settings.advanced.modelUnload.title")}
      description={
        isCloudModelActive
          ? t("settings.advanced.modelUnload.cloudNote")
          : t("settings.advanced.modelUnload.description")
      }
      descriptionMode={descriptionMode}
      grouped={grouped}
    >
      <Dropdown
        options={options}
        selectedValue={currentValue}
        onSelect={(value) =>
          handleChange({
            target: { value },
          } as React.ChangeEvent<HTMLSelectElement>)
        }
        disabled={false}
      />
    </SettingContainer>
  );
};
