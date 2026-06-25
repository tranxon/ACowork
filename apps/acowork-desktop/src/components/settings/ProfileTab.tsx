import { useState, useEffect, useCallback } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { UserAvatar, BUILTIN_ICONS, BUILTIN_ICON_IDS } from "../common/UserAvatar";
import { fetchActiveUser, updateUser } from "../../lib/gateway-api";
import type { BackendUserProfile, AvatarAssetEntry } from "../../lib/types";
import {
  fetchUserAvatarConfig,
  updateUserAvatarConfig,
  fetchUserAvatarAssets,
  deleteUserAvatarFile,
  resolveUserAvatarFileUrl,
} from "../../lib/avatar";
import type { UserAvatarConfig } from "../../lib/avatar";
import { useTranslation } from "../../i18n/useTranslation";
import { StyledInput } from "../common/StyledInput";
import i18n from "../../i18n";

const TIMEZONES = [
  "Asia/Shanghai",
  "Asia/Tokyo",
  "America/New_York",
  "America/Los_Angeles",
  "Europe/London",
  "UTC",
];

const IMAGE_EXTENSIONS = ["png", "jpg", "jpeg", "gif", "webp", "svg"];

// ── Component ───────────────────────────────────────────────────────────

export function ProfileTab() {
  const { t } = useTranslation();
  const { profile, setProfile } = useUserProfileStore();
  const [nameValue, setNameValue] = useState(profile.displayName);

  // ── Avatar state ───────────────────────────────────────────────────
  const [avatarPopupOpen, setAvatarPopupOpen] = useState(false);
  const [avatarTab, setAvatarTab] = useState<"custom" | "builtin">("builtin");
  const [avatarConfig, setAvatarConfig] = useState<UserAvatarConfig | null>(null);
  const [avatarAssets, setAvatarAssets] = useState<AvatarAssetEntry[]>([]);
  const [avatarBusy, setAvatarBusy] = useState(false);

  const languages = [
    { value: "zh-CN", label: t("language.zhCN") },
    { value: "zh-TW", label: t("language.zhTW") },
    { value: "en", label: t("language.en") },
    { value: "ja", label: t("language.ja") },
    { value: "ko", label: t("language.ko") },
  ];

  // ── Load avatar config from backend ────────────────────────────────
  useEffect(() => {
    fetchUserAvatarConfig()
      .then((cfg) => {
        setAvatarConfig(cfg);
        setProfile({
          backendAvatarUrl: cfg.avatar ?? null,
          backendBuiltinAvatarId: cfg.builtin_avatar ?? null,
        });
      })
      .catch(() => {});
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  // Refresh avatar assets when popup opens
  useEffect(() => {
    if (!avatarPopupOpen) return;
    fetchUserAvatarAssets()
      .then((r) => setAvatarAssets(r.assets))
      .catch(() => {});
  }, [avatarPopupOpen]);

  // ── Load backend user profile ──────────────────────────────────────
  const [backendUser, setBackendUser] = useState<BackendUserProfile | null>(null);
  const [backendLoading, setBackendLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [savedMsg, setSavedMsg] = useState<string | null>(null);

  // Form draft state for backend fields
  const [language, setLanguage] = useState("zh-CN");
  const [timezone, setTimezone] = useState("Asia/Shanghai");
  const [city, setCity] = useState("");
  const [occupation, setOccupation] = useState("");

  // Load active user from backend on mount
  useEffect(() => {
    let cancelled = false;
    fetchActiveUser()
      .then((user) => {
        if (cancelled) return;
        setBackendUser(user);
        if (user) {
          setLanguage(user.language);
          if (user.language) {
            i18n.changeLanguage(user.language);
          }
          setTimezone(user.timezone);
          setCity(user.city ?? "");
          setOccupation(user.occupation ?? "");
          // Sync display name from backend to local store
          if (user.display_name) {
            setProfile({ displayName: user.display_name });
            setNameValue(user.display_name);
          }
        }
      })
      .catch(() => {
        // Gateway not reachable or no users yet — use local state
      })
      .finally(() => {
        if (!cancelled) setBackendLoading(false);
      });
    return () => { cancelled = true; };
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  // ── Save helpers ──────────────────────────────────────────────────

  /** Save a single field to backend (debounced via onBlur) */
  const saveField = useCallback(async (userId: string, field: string, value: string) => {
    setSaving(true);
    setSavedMsg(null);
    try {
      const updated = await updateUser(userId, { [field]: value });
      setBackendUser(updated);
      setSavedMsg("saved");
      setTimeout(() => setSavedMsg(null), 2000);
    } catch (err) {
      console.warn(`Failed to save ${field}:`, err);
      setSavedMsg("failed");
    } finally {
      setSaving(false);
    }
  }, []);

  /** Save display name to both backend and local store */
  const saveDisplayName = useCallback((value: string) => {
    const trimmed = value.trim();
    if (!trimmed) return;
    // Always update local store
    setProfile({ displayName: trimmed });
    // Also update backend if we have a user
    if (backendUser) {
      saveField(backendUser.user_id, "display_name", trimmed);
    }
  }, [backendUser, setProfile, saveField]);

  // ── Avatar handlers ───────────────────────────────────────────────

  const handleSelectCustom = async (relativePath: string) => {
    setAvatarBusy(true);
    try {
      const cfg = await updateUserAvatarConfig({ avatar: relativePath, builtin_avatar: "" });
      setAvatarConfig(cfg);
      setProfile({
        backendAvatarUrl: cfg.avatar ?? null,
        backendBuiltinAvatarId: cfg.builtin_avatar ?? null,
      });
    } catch (err) {
      console.warn("[ProfileTab] Select custom avatar failed:", err);
    } finally {
      setAvatarBusy(false);
      setAvatarPopupOpen(false);
    }
  };

  const handleSelectBuiltin = async (iconId: string) => {
    setAvatarBusy(true);
    try {
      const cfg = await updateUserAvatarConfig({ avatar: "", builtin_avatar: iconId });
      setAvatarConfig(cfg);
      setProfile({
        backendAvatarUrl: cfg.avatar ?? null,
        backendBuiltinAvatarId: cfg.builtin_avatar ?? null,
      });
    } catch (err) {
      console.warn("[ProfileTab] Select builtin avatar failed:", err);
    } finally {
      setAvatarBusy(false);
      setAvatarPopupOpen(false);
    }
  };

  const handleUploadClick = async () => {
    const selected = await openDialog({
      multiple: false,
      filters: [{ name: "Images", extensions: IMAGE_EXTENSIONS }],
    });
    if (!selected || typeof selected !== "string") return;

    setAvatarBusy(true);
    try {
      await invoke("upload_user_avatar_file", { filePath: selected });
      const resp = await fetchUserAvatarAssets();
      setAvatarAssets(resp.assets);
    } catch (err) {
      console.warn("[ProfileTab] Avatar upload failed:", err);
    } finally {
      setAvatarBusy(false);
    }
  };

  const handleDeleteAvatar = async (relativePath: string) => {
    setAvatarBusy(true);
    try {
      await deleteUserAvatarFile(relativePath);
      const [assetsResp, cfg] = await Promise.all([
        fetchUserAvatarAssets(),
        fetchUserAvatarConfig(),
      ]);
      setAvatarAssets(assetsResp.assets);
      setAvatarConfig(cfg);
    } catch (err) {
      console.warn("[ProfileTab] Delete avatar failed:", err);
    } finally {
      setAvatarBusy(false);
      setAvatarPopupOpen(false);
    }
  };

  // ── Render ────────────────────────────────────────────────────────

  return (
    <div className="max-w-lg space-y-4">
      {/* ── Avatar & Display Name ────────────────────────────────── */}
      <div className="rounded-md border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <h2 className="mb-3 text-xs font-medium">{t("settings.profileTitle")}</h2>

        {/* Avatar preview — click to open picker popup */}
        <div className="flex items-center gap-4">
          <div className="relative">
            <button
              onClick={() => setAvatarPopupOpen((v) => !v)}
              className="relative block rounded-full ring-1 ring-zinc-300/60 transition hover:ring-zinc-400 dark:ring-zinc-600/60 dark:hover:ring-zinc-400"
            >
              <UserAvatar
                size={64}
                avatarUrl={avatarConfig?.avatar ?? null}
                builtinAvatarId={avatarConfig?.builtin_avatar ?? null}
              />
              {/* Pencil badge */}
              <span className="absolute -bottom-0.5 -right-0.5 flex h-5 w-5 items-center justify-center rounded-full bg-zinc-800 text-white shadow-sm dark:bg-zinc-600">
                <svg viewBox="0 0 16 16" className="h-3 w-3 fill-current" xmlns="http://www.w3.org/2000/svg">
                  <path d="M11.013 1.427a1.75 1.75 0 0 1 2.474 0l1.086 1.086a1.75 1.75 0 0 1 0 2.474l-8.61 8.61c-.21.21-.47.364-.756.445l-3.251.93a.75.75 0 0 1-.927-.928l.929-3.25c.081-.286.235-.547.445-.758l8.61-8.61Zm.176 4.823L11.5 7l-3-3-.31.31a.75.75 0 0 0-.177.764l.93 3.251a.75.75 0 0 1-.927.928l-3.251-.93Z" />
                </svg>
              </span>
            </button>

            {/* Avatar picker popup */}
            {avatarPopupOpen && (
              <>
                {/* Click-outside overlay */}
                <div
                  className="fixed inset-0 z-40"
                  onClick={() => setAvatarPopupOpen(false)}
                />
                <div className="absolute left-0 top-full z-50 mt-2 w-72 rounded-lg border border-zinc-200 bg-white p-3 shadow-lg dark:border-zinc-700 dark:bg-zinc-800">
                  {/* Tabs */}
                  <div className="mb-3 flex gap-1 border-b border-zinc-200 dark:border-zinc-700">
                    <button
                      onClick={() => setAvatarTab("custom")}
                      className={`px-3 py-1 text-xs font-medium transition-colors ${avatarTab === "custom"
                        ? "border-b-2 border-zinc-800 text-zinc-800 dark:border-zinc-200 dark:text-zinc-200"
                        : "text-zinc-400 hover:text-zinc-600 dark:text-zinc-500"
                        }`}
                    >
                      Custom
                    </button>
                    <button
                      onClick={() => setAvatarTab("builtin")}
                      className={`px-3 py-1 text-xs font-medium transition-colors ${avatarTab === "builtin"
                        ? "border-b-2 border-zinc-800 text-zinc-800 dark:border-zinc-200 dark:text-zinc-200"
                        : "text-zinc-400 hover:text-zinc-600 dark:text-zinc-500"
                        }`}
                    >
                      Builtin
                    </button>
                  </div>

                  {/* Custom tab */}
                  {avatarTab === "custom" && (
                    <div className="grid grid-cols-4 gap-2">
                      <button
                        onClick={handleUploadClick}
                        disabled={avatarBusy}
                        className="flex aspect-square items-center justify-center rounded-md border border-dashed border-zinc-300 text-zinc-400 transition-colors hover:border-zinc-400 hover:text-zinc-600 disabled:opacity-50 dark:border-zinc-600 dark:text-zinc-500 dark:hover:border-zinc-400"
                      >
                        <span className="text-lg">+</span>
                      </button>
                      {avatarAssets.map((asset) => {
                        const isSelected = avatarConfig?.avatar === asset.relative_path;
                        return (
                          <div
                            key={asset.relative_path}
                            className={`group relative aspect-square overflow-hidden rounded-md border-2 transition-colors ${isSelected
                              ? "border-zinc-800 dark:border-zinc-200"
                              : "border-transparent hover:border-zinc-300 dark:hover:border-zinc-600"
                              }`}
                          >
                            <img
                              src={resolveUserAvatarFileUrl(asset.relative_path)}
                              alt={asset.relative_path}
                              draggable={false}
                              className="h-full w-full cursor-pointer object-cover"
                              onClick={() => handleSelectCustom(asset.relative_path)}
                            />
                            <button
                              onClick={(e) => {
                                e.stopPropagation();
                                handleDeleteAvatar(asset.relative_path);
                              }}
                              disabled={avatarBusy}
                              className="absolute right-0.5 top-0.5 flex h-4 w-4 items-center justify-center rounded bg-red-500/80 text-[8px] text-white opacity-0 transition-opacity group-hover:opacity-100"
                            >
                              ×
                            </button>
                          </div>
                        );
                      })}
                    </div>
                  )}

                  {/* Builtin tab */}
                  {avatarTab === "builtin" && (
                    <div className="grid grid-cols-4 gap-2">
                      {BUILTIN_ICON_IDS.map((iconId) => {
                        const isSelected = avatarConfig?.builtin_avatar === iconId;
                        return (
                          <button
                            key={iconId}
                            onClick={() => handleSelectBuiltin(iconId)}
                            disabled={avatarBusy}
                            className={`flex items-center justify-center rounded-md p-1 transition-colors ${isSelected
                              ? "bg-zinc-200 dark:bg-zinc-600"
                              : "hover:bg-zinc-100 dark:hover:bg-zinc-700"
                              }`}
                          >
                            <img
                              src={BUILTIN_ICONS[iconId] ?? ""}
                              alt={iconId}
                              draggable={false}
                              className="h-12 w-12 rounded-full object-cover"
                            />
                          </button>
                        );
                      })}
                    </div>
                  )}
                </div>
              </>
            )}
          </div>
          <div>
            <p className="text-sm font-medium text-zinc-800 dark:text-zinc-200">
              {profile.displayName}
            </p>
          </div>
        </div>

        {/* Display name */}
        <div className="mt-3 space-y-1.5">
          <label className="block text-xs font-medium text-zinc-600 dark:text-zinc-400">
            {t("settings.displayName")}
          </label>
          <StyledInput
            type="text"
            value={nameValue}
            onChange={(e) => setNameValue(e.target.value)}
            onBlur={() => saveDisplayName(nameValue)}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                saveDisplayName(nameValue);
                (e.target as HTMLInputElement).blur();
              }
            }}
            placeholder={t("settings.displayNamePlaceholder")}
            className="rounded border-zinc-300 bg-white py-2 text-zinc-800 placeholder:text-zinc-400 dark:border-zinc-600 dark:bg-zinc-800 dark:placeholder:text-zinc-500"
          />
        </div>
      </div>

      {/* ── Backend Identity Fields ───────────────────────────────── */}
      <div className="rounded-md border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <div className="flex items-center justify-between mb-3">
          <h2 className="text-xs font-medium">{t("settings.identityTitle")}</h2>
          {savedMsg && (
            <span className={`text-[10px] ${savedMsg === "saved" ? "text-[var(--color-accent)]" : "text-red-500"}`}>
              {savedMsg === "saved" ? t("settings.saved") : t("settings.saveFailed")}
            </span>
          )}
          {saving && <span className="text-[10px] text-zinc-400">{t("settings.saving")}</span>}
        </div>

        {backendLoading ? (
          <p className="text-xs text-zinc-400">{t("settings.loading")}</p>
        ) : backendUser ? (
          <div className="space-y-3">
            {/* Language */}
            <div>
              <label className="mb-1 block text-xs text-zinc-500">{t("settings.language")}</label>
              <select
                value={language}
                onChange={(e) => {
                  const lng = e.target.value;
                  console.log("[ProfileTab] Switching language to:", lng);
                  setLanguage(lng);
                  i18n.changeLanguage(lng).then(() => {
                    console.log("[ProfileTab] changeLanguage resolved, i18n.language =", i18n.language);
                  });
                  if (backendUser?.user_id) {
                    saveField(backendUser.user_id, "language", lng);
                  }
                }}
                className="w-full rounded border border-zinc-200 bg-white px-2.5 py-1.5 text-xs text-zinc-800 focus:border-zinc-400 focus:outline-none focus:ring-1 focus:ring-zinc-400 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
                style={{
                  backgroundImage: `url("data:image/svg+xml,%3csvg xmlns='http://www.w3.org/2000/svg' fill='none' viewBox='0 0 20 20'%3e%3cpath stroke='%236b7280' stroke-linecap='round' stroke-linejoin='round' stroke-width='1.5' d='M6 8l4 4 4-4'/%3e%3c/svg%3e")`,
                  backgroundPosition: 'right 0.5rem center',
                  backgroundRepeat: 'no-repeat',
                  backgroundSize: '1.5em 1.5em',
                  paddingRight: '2rem',
                  appearance: 'none',
                  WebkitAppearance: 'none',
                  MozAppearance: 'none',
                }}
              >
                {languages.map((l) => (
                  <option key={l.value} value={l.value}>{l.label}</option>
                ))}
              </select>
            </div>

            {/* Timezone */}
            <div>
              <label className="mb-1 block text-xs text-zinc-500">{t("settings.timezone")}</label>
              <select
                value={timezone}
                onChange={(e) => {
                  setTimezone(e.target.value);
                  saveField(backendUser.user_id, "timezone", e.target.value);
                }}
                className="w-full rounded border border-zinc-200 bg-white px-2.5 py-1.5 text-xs text-zinc-800 focus:border-zinc-400 focus:outline-none focus:ring-1 focus:ring-zinc-400 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
                style={{
                  backgroundImage: `url("data:image/svg+xml,%3csvg xmlns='http://www.w3.org/2000/svg' fill='none' viewBox='0 0 20 20'%3e%3cpath stroke='%236b7280' stroke-linecap='round' stroke-linejoin='round' stroke-width='1.5' d='M6 8l4 4 4-4'/%3e%3c/svg%3e")`,
                  backgroundPosition: 'right 0.5rem center',
                  backgroundRepeat: 'no-repeat',
                  backgroundSize: '1.5em 1.5em',
                  paddingRight: '2rem',
                  appearance: 'none',
                  WebkitAppearance: 'none',
                  MozAppearance: 'none',
                }}
              >
                {TIMEZONES.map((tz) => (
                  <option key={tz} value={tz}>{tz}</option>
                ))}
              </select>
            </div>

            {/* City */}
            <div>
              <label className="mb-1 block text-xs text-zinc-500">{t("settings.city")}</label>
              <StyledInput
                type="text"
                value={city}
                onChange={(e) => setCity(e.target.value)}
                onBlur={() => saveField(backendUser.user_id, "city", city.trim())}
                onKeyDown={(e) => {
                  if (e.key === "Enter") {
                    saveField(backendUser.user_id, "city", city.trim());
                    (e.target as HTMLInputElement).blur();
                  }
                }}
                placeholder={t("settings.cityPlaceholder")}
                className="rounded border-zinc-300 bg-white py-2 text-zinc-800 placeholder:text-zinc-400 dark:border-zinc-600 dark:bg-zinc-800 dark:placeholder:text-zinc-500"
              />
            </div>

            {/* Occupation */}
            <div>
              <label className="mb-1 block text-xs text-zinc-500">{t("settings.occupation")}</label>
              <StyledInput
                type="text"
                value={occupation}
                onChange={(e) => setOccupation(e.target.value)}
                onBlur={() => saveField(backendUser.user_id, "occupation", occupation.trim())}
                onKeyDown={(e) => {
                  if (e.key === "Enter") {
                    saveField(backendUser.user_id, "occupation", occupation.trim());
                    (e.target as HTMLInputElement).blur();
                  }
                }}
                placeholder={t("settings.occupationPlaceholder")}
                className="rounded border-zinc-300 bg-white py-2 text-zinc-800 placeholder:text-zinc-400 dark:border-zinc-600 dark:bg-zinc-800 dark:placeholder:text-zinc-500"
              />
            </div>
          </div>
        ) : (
          <p className="text-xs text-zinc-400">
            {t("settings.noProfile")}
          </p>
        )}
      </div>
    </div>
  );
}
