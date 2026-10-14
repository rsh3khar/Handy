import React from "react";
import { useTranslation } from "react-i18next";
import { ToggleSwitch } from "../../ui/ToggleSwitch";
import { useSettings } from "../../../hooks/useSettings";
import { useOsType } from "../../../hooks/useOsType";

interface ReliablePasteToggleProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
}

export const ReliablePasteToggle: React.FC<ReliablePasteToggleProps> = ({
  descriptionMode = "tooltip",
  grouped = false,
}) => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, isUpdating } = useSettings();
  const osType = useOsType();

  // macOS always uses the guarded native path; the experimental switch now
  // applies only to Windows.
  if (osType !== "windows") {
    return null;
  }

  return (
    <ToggleSwitch
      checked={getSetting("reliable_paste") ?? false}
      onChange={(enabled) => updateSetting("reliable_paste", enabled)}
      isUpdating={isUpdating("reliable_paste")}
      label={t("settings.debug.reliablePaste.title")}
      description={t("settings.debug.reliablePaste.description")}
      descriptionMode={descriptionMode}
      grouped={grouped}
    />
  );
};
