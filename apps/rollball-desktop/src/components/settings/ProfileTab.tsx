import { useState, useEffect, useCallback } from "react";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { UserAvatar, BUILTIN_ICONS, BUILTIN_ICON_IDS } from "../common/UserAvatar";
import { ConfirmDialog } from "../common/ConfirmDialog";
import { RadioGroup } from "../common/RadioGroup";
import { fetchActiveUser, updateUser } from "../../lib/gateway-api";
import type { BackendUserProfile } from "../../lib/types";

const LANGUAGES = [
  { value: "zh-CN", label: "中文 (简体)" },
  { value: "zh-TW", label: "中文 (繁體)" },
  { value: "en", label: "English" },
  { value: "ja", label: "日本語" },
  { value: "ko", label: "한국어" },
];

const TIMEZONES = [
  "Asia/Shanghai",
  "Asia/Tokyo",
  "America/New_York",
  "America/Los_Angeles",
  "Europe/London",
  "UTC",
];

// ── Component ───────────────────────────────────────────────────────────

export function ProfileTab() {
  const { profile, setProfile, resetProfile } = useUserProfileStore();
  const [nameValue, setNameValue] = useState(profile.displayName);
  const [iconOpen, setIconOpen] = useState(false);
  const [showResetConfirm, setShowResetConfirm] = useState(false);

  // ── Backend user profile state ─────────────────────────────────────
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
      setSavedMsg("Saved");
      setTimeout(() => setSavedMsg(null), 2000);
    } catch (err) {
      console.warn(`Failed to save ${field}:`, err);
      setSavedMsg("Save failed");
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

  // ── Render ────────────────────────────────────────────────────────

  return (
    <div className="max-w-lg space-y-4">
      {/* ── Avatar & Display Name ────────────────────────────────── */}
      <div className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <h2 className="mb-3 text-xs font-medium">Your Profile</h2>

        {/* Live avatar preview — click to open icon picker */}
        <div className="flex items-center gap-4">
          <div className="relative">
            <button
              onClick={() => setIconOpen(!iconOpen)}
              className="rounded-lg border border-transparent p-0.5 transition-colors hover:border-zinc-300 dark:hover:border-zinc-600"
              title="Choose icon"
            >
              <UserAvatar size={64} />
            </button>
            {iconOpen && (
              <div className="absolute left-0 z-50 mt-1 w-max rounded-lg border border-zinc-200 bg-white p-1.5 shadow-lg dark:border-zinc-700 dark:bg-zinc-800">
                <div className="grid grid-cols-4 gap-1">
                  {BUILTIN_ICON_IDS.map((iconId) => (
                    <button
                      key={iconId}
                      onClick={() => {
                        setProfile({ avatarIcon: iconId });
                        setIconOpen(false);
                      }}
                      className={`flex items-center justify-center rounded-md p-1 transition-colors ${
                        profile.avatarIcon === iconId
                          ? "bg-zinc-200 dark:bg-zinc-600"
                          : "hover:bg-zinc-100 dark:hover:bg-zinc-700"
                      }`}
                    >
                      <img
                        src={BUILTIN_ICONS[iconId] ?? ""}
                        alt={iconId}
                        draggable={false}
                        className="h-16 w-16 rounded-full object-cover"
                      />
                    </button>
                  ))}
                </div>
              </div>
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
            Display Name
          </label>
          <input
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
            placeholder="Your display name"
            className="w-full rounded-lg border border-zinc-300 bg-white px-3 py-2 text-xs text-zinc-800 placeholder:text-zinc-400 focus:border-zinc-400 focus:outline-none focus:ring-1 focus:ring-zinc-400 dark:border-zinc-600 dark:bg-zinc-800 dark:text-zinc-200 dark:placeholder:text-zinc-500"
          />
        </div>
      </div>

      {/* ── Backend Identity Fields ───────────────────────────────── */}
      <div className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <div className="flex items-center justify-between mb-3">
          <h2 className="text-xs font-medium">Identity (shared with Agents)</h2>
          {savedMsg && (
            <span className={`text-[10px] ${savedMsg === "Saved" ? "text-[var(--color-accent)]" : "text-red-500"}`}>
              {savedMsg}
            </span>
          )}
          {saving && <span className="text-[10px] text-zinc-400">Saving...</span>}
        </div>

        {backendLoading ? (
          <p className="text-xs text-zinc-400">Loading...</p>
        ) : backendUser ? (
          <div className="space-y-3">
            {/* Language */}
            <div>
              <label className="mb-1 block text-xs text-zinc-500">Language</label>
              <select
                value={language}
                onChange={(e) => {
                  setLanguage(e.target.value);
                  saveField(backendUser.user_id, "language", e.target.value);
                }}
                className="w-full rounded-lg border border-zinc-200 bg-white px-2.5 py-1.5 text-xs text-zinc-800 focus:border-zinc-400 focus:outline-none focus:ring-1 focus:ring-zinc-400 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
                style={{
                  backgroundImage: `url("data:image/svg+xml,%3csvg xmlns='http://www.w3.org/2000/svg' fill='none' viewBox='0 0 20 20'%3e%3cpath stroke='%236b7280' stroke-linecap='round' stroke-linejoin='round' stroke-width='1.5' d='M6 8l4 4 4-4'/%3e%3c/svg%3e")`,
                  backgroundPosition: 'right 0.5rem center',
                  backgroundRepeat: 'no-repeat',
                  backgroundSize: '1.5em 1.5em',
                  paddingRight: '2rem',
                }}
              >
                {LANGUAGES.map((l) => (
                  <option key={l.value} value={l.value}>{l.label}</option>
                ))}
              </select>
            </div>

            {/* Timezone */}
            <div>
              <label className="mb-1 block text-xs text-zinc-500">Timezone</label>
              <select
                value={timezone}
                onChange={(e) => {
                  setTimezone(e.target.value);
                  saveField(backendUser.user_id, "timezone", e.target.value);
                }}
                className="w-full rounded-lg border border-zinc-200 bg-white px-2.5 py-1.5 text-xs text-zinc-800 focus:border-zinc-400 focus:outline-none focus:ring-1 focus:ring-zinc-400 dark:border-zinc-700 dark:bg-zinc-800 dark:text-zinc-200"
                style={{
                  backgroundImage: `url("data:image/svg+xml,%3csvg xmlns='http://www.w3.org/2000/svg' fill='none' viewBox='0 0 20 20'%3e%3cpath stroke='%236b7280' stroke-linecap='round' stroke-linejoin='round' stroke-width='1.5' d='M6 8l4 4 4-4'/%3e%3c/svg%3e")`,
                  backgroundPosition: 'right 0.5rem center',
                  backgroundRepeat: 'no-repeat',
                  backgroundSize: '1.5em 1.5em',
                  paddingRight: '2rem',
                }}
              >
                {TIMEZONES.map((tz) => (
                  <option key={tz} value={tz}>{tz}</option>
                ))}
              </select>
            </div>

            {/* City */}
            <div>
              <label className="mb-1 block text-xs text-zinc-500">City (optional)</label>
              <input
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
                placeholder="e.g. Shanghai"
                className="w-full rounded-lg border border-zinc-300 bg-white px-3 py-2 text-xs text-zinc-800 placeholder:text-zinc-400 focus:border-zinc-400 focus:outline-none focus:ring-1 focus:ring-zinc-400 dark:border-zinc-600 dark:bg-zinc-800 dark:text-zinc-200 dark:placeholder:text-zinc-500"
              />
            </div>

            {/* Occupation */}
            <div>
              <label className="mb-1 block text-xs text-zinc-500">Occupation (optional)</label>
              <input
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
                placeholder="e.g. Software Engineer"
                className="w-full rounded-lg border border-zinc-300 bg-white px-3 py-2 text-xs text-zinc-800 placeholder:text-zinc-400 focus:border-zinc-400 focus:outline-none focus:ring-1 focus:ring-zinc-400 dark:border-zinc-600 dark:bg-zinc-800 dark:text-zinc-200 dark:placeholder:text-zinc-500"
              />
            </div>
          </div>
        ) : (
          <p className="text-xs text-zinc-400">
            No user profile found on Gateway. Complete the onboarding to create one, or ensure Gateway is running.
          </p>
        )}
      </div>

      {/* ── Avatar Customization ───────────────────────────────────── */}
      <div className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <h2 className="mb-3 text-xs font-medium">Avatar Customization</h2>

        <div className="space-y-1.5">
          <label className="block text-xs font-medium text-zinc-600 dark:text-zinc-400">
            Avatar Type
          </label>
          <RadioGroup
            name="avatarType"
            value={profile.avatarType}
            options={[
              { label: "Built-in Icon", value: "icon" as const },
              { label: "Letter", value: "letter" as const },
            ]}
            onChange={(type) => setProfile({ avatarType: type })}
          />
        </div>
      </div>

      <div className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <button
          onClick={() => setShowResetConfirm(true)}
          className="rounded-lg btn-solid px-3 py-1.5 text-xs"
        >
          Reset to defaults
        </button>

        <ConfirmDialog
          open={showResetConfirm}
          title="Reset Profile"
          message="确定要重置个人资料到默认值吗？"
          confirmLabel="Reset"
          destructive
          onConfirm={() => {
            resetProfile();
            setNameValue("我");
            setShowResetConfirm(false);
          }}
          onCancel={() => setShowResetConfirm(false)}
        />
      </div>
    </div>
  );
}
