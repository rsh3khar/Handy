import React, { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Upload, Copy, Check, Loader2, AlertCircle } from "lucide-react";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { open } from "@tauri-apps/plugin-dialog";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";

interface FileTranscriptionResult {
  text: string;
  file_name: string;
  duration_ms: number;
}

interface FileTranscriptionProgress {
  stage: string;
  message: string | null;
}

type TranscribeState =
  | { kind: "idle" }
  | { kind: "processing"; stage: string }
  | { kind: "result"; result: FileTranscriptionResult }
  | { kind: "error"; message: string };

const SUPPORTED_EXTENSIONS = ["wav", "mp3", "flac", "m4a", "aac", "ogg", "oga"];

function getExtension(path: string): string {
  const parts = path.split(".");
  return parts.length > 1 ? parts[parts.length - 1].toLowerCase() : "";
}

export const TranscribeSettings: React.FC = () => {
  const { t } = useTranslation();
  const [state, setState] = useState<TranscribeState>({ kind: "idle" });
  const [isDragOver, setIsDragOver] = useState(false);
  const [copied, setCopied] = useState(false);
  const copyTimeoutRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const transcribeFile = useCallback(
    async (filePath: string) => {
      const ext = getExtension(filePath);
      if (!SUPPORTED_EXTENSIONS.includes(ext)) {
        setState({
          kind: "error",
          message: t("settings.transcribe.unsupportedFormat"),
        });
        return;
      }

      setState({ kind: "processing", stage: "decoding" });

      try {
        const result = await invoke<FileTranscriptionResult>(
          "transcribe_audio_file",
          { filePath },
        );

        if (!result.text || result.text.trim() === "") {
          setState({
            kind: "result",
            result: { ...result, text: "" },
          });
        } else {
          setState({ kind: "result", result });
        }
      } catch (e) {
        setState({
          kind: "error",
          message: typeof e === "string" ? e : String(e),
        });
      }
    },
    [t],
  );

  // Listen for progress events
  useEffect(() => {
    const unlisten = listen<FileTranscriptionProgress>(
      "file-transcription-progress",
      (event) => {
        setState((prev) => {
          if (prev.kind === "processing") {
            return { kind: "processing", stage: event.payload.stage };
          }
          return prev;
        });
      },
    );

    return () => {
      unlisten.then((fn) => fn());
    };
  }, []);

  // Listen for drag-and-drop events
  useEffect(() => {
    const webview = getCurrentWebviewWindow();
    const unlisten = webview.onDragDropEvent((event) => {
      if (event.payload.type === "over") {
        setIsDragOver(true);
      } else if (event.payload.type === "drop") {
        setIsDragOver(false);
        const paths = event.payload.paths;
        if (paths.length > 0) {
          transcribeFile(paths[0]);
        }
      } else if (event.payload.type === "leave") {
        setIsDragOver(false);
      }
    });

    return () => {
      unlisten.then((fn) => fn());
    };
  }, [transcribeFile]);

  // Cleanup copy timeout
  useEffect(() => {
    return () => {
      if (copyTimeoutRef.current) {
        clearTimeout(copyTimeoutRef.current);
      }
    };
  }, []);

  const handleChooseFile = async () => {
    const selected = await open({
      multiple: false,
      filters: [
        {
          name: "Audio Files",
          extensions: SUPPORTED_EXTENSIONS,
        },
      ],
    });
    if (selected) {
      transcribeFile(selected);
    }
  };

  const handleCopy = async () => {
    if (state.kind === "result" && state.result.text) {
      await navigator.clipboard.writeText(state.result.text);
      setCopied(true);
      if (copyTimeoutRef.current) {
        clearTimeout(copyTimeoutRef.current);
      }
      copyTimeoutRef.current = setTimeout(() => setCopied(false), 2000);
    }
  };

  const stageKey =
    state.kind === "processing"
      ? `settings.transcribe.stage.${state.stage}`
      : "";

  return (
    <div className="max-w-3xl w-full mx-auto space-y-6">
      <div className="space-y-2">
        <div className="px-4">
          <h2 className="text-xs font-medium text-mid-gray uppercase tracking-wide">
            {t("settings.transcribe.title")}
          </h2>
        </div>

        <div className="bg-background border border-mid-gray/20 rounded-lg p-6">
          {/* Drop zone */}
          <div
            className={`flex flex-col items-center justify-center gap-3 p-8 border-2 border-dashed rounded-lg transition-colors ${
              isDragOver
                ? "border-logo-primary bg-logo-primary/10"
                : "border-mid-gray/30 hover:border-mid-gray/50"
            } ${state.kind === "processing" ? "pointer-events-none opacity-60" : "cursor-pointer"}`}
            onClick={state.kind !== "processing" ? handleChooseFile : undefined}
          >
            {state.kind === "processing" ? (
              <>
                <Loader2 className="w-8 h-8 text-logo-primary animate-spin" />
                <p className="text-sm text-mid-gray">{t(stageKey)}</p>
              </>
            ) : (
              <>
                <Upload className="w-8 h-8 text-mid-gray/50" />
                <p className="text-sm text-mid-gray">
                  {t("settings.transcribe.dropZoneText")}
                </p>
                <button
                  onClick={(e) => {
                    e.stopPropagation();
                    handleChooseFile();
                  }}
                  className="px-4 py-1.5 text-sm bg-logo-primary/80 hover:bg-logo-primary rounded-lg transition-colors"
                >
                  {t("settings.transcribe.chooseFile")}
                </button>
                <p className="text-xs text-mid-gray/50">
                  {t("settings.transcribe.supportedFormats")}
                </p>
              </>
            )}
          </div>

          {/* Result */}
          {state.kind === "result" && (
            <div className="mt-4 space-y-2">
              <div className="flex items-center justify-between">
                <h3 className="text-xs font-medium text-mid-gray uppercase tracking-wide">
                  {t("settings.transcribe.result")}
                </h3>
                {state.result.text && (
                  <button
                    onClick={handleCopy}
                    className="flex items-center gap-1 px-2 py-1 text-xs text-mid-gray hover:text-white rounded transition-colors"
                  >
                    {copied ? (
                      <>
                        <Check className="w-3 h-3" />
                        {t("settings.transcribe.copied")}
                      </>
                    ) : (
                      <>
                        <Copy className="w-3 h-3" />
                        {t("settings.transcribe.copy")}
                      </>
                    )}
                  </button>
                )}
              </div>
              <div className="bg-background border border-mid-gray/20 rounded-lg p-4">
                <p className="text-sm select-text whitespace-pre-wrap">
                  {state.result.text ||
                    t("settings.transcribe.noSpeechDetected")}
                </p>
              </div>
              <p className="text-xs text-mid-gray/50">
                {state.result.file_name} &mdash; {state.result.duration_ms}
                {"ms"}
              </p>
            </div>
          )}

          {/* Error */}
          {state.kind === "error" && (
            <div className="mt-4 flex items-start gap-2 p-3 bg-red-500/10 border border-red-500/20 rounded-lg">
              <AlertCircle className="w-4 h-4 text-red-400 shrink-0 mt-0.5" />
              <p className="text-sm text-red-400">{state.message}</p>
            </div>
          )}
        </div>
      </div>
    </div>
  );
};
