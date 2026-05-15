import { useState } from "react";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { UserAvatar, BUILTIN_ICONS, BUILTIN_ICON_IDS } from "../common/UserAvatar";
import type { BoringAvatarVariant, ColorPalette } from "../../lib/types";
import { COLOR_PALETTES } from "../../lib/types";

// ── Constants ───────────────────────────────────────────────────────────

const VARIANTS: { id: BoringAvatarVariant; label: string }[] = [
  { id: "beam", label: "Beam" },
  { id: "marble", label: "Marble" },
  { id: "pixel", label: "Pixel" },
  { id: "sunset", label: "Sunset" },
  { id: "ring", label: "Ring" },
  { id: "bauhaus", label: "Bauhaus" },
];

const PALETTES: { id: ColorPalette; label: string }[] = [
  { id: "rainbow", label: "Rainbow" },
  { id: "ocean", label: "Ocean" },
  { id: "forest", label: "Forest" },
  { id: "sunset", label: "Sunset" },
  { id: "neon", label: "Neon" },
];

// ── Component ───────────────────────────────────────────────────────────

export function ProfileTab() {
  const { profile, setProfile, resetProfile } = useUserProfileStore();
  const [nameValue, setNameValue] = useState(profile.displayName);
  const [iconOpen, setIconOpen] = useState(false);

  return (
    <div className="max-w-lg space-y-6">
      <h2 className="text-sm font-medium">Your Profile</h2>

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
                      className="h-10 w-10 rounded-full object-cover"
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
      <div className="space-y-1.5">
        <label className="block text-xs font-medium text-zinc-600 dark:text-zinc-400">
          Display Name
        </label>
        <input
          type="text"
          value={nameValue}
          onChange={(e) => setNameValue(e.target.value)}
          onBlur={() => {
            if (nameValue.trim()) setProfile({ displayName: nameValue.trim() });
          }}
          onKeyDown={(e) => {
            if (e.key === "Enter" && nameValue.trim()) {
              setProfile({ displayName: nameValue.trim() });
              (e.target as HTMLInputElement).blur();
            }
          }}
          placeholder="Your display name"
          className="w-full rounded-lg border border-zinc-300 bg-white px-3 py-2 text-sm text-zinc-800 placeholder:text-zinc-400 focus:border-zinc-400 focus:outline-none focus:ring-1 focus:ring-zinc-400 dark:border-zinc-600 dark:bg-zinc-800 dark:text-zinc-200 dark:placeholder:text-zinc-500"
        />
      </div>

      {/* Avatar type selection */}
      <div className="space-y-1.5">
        <label className="block text-xs font-medium text-zinc-600 dark:text-zinc-400">
          Avatar Type
        </label>
        <div className="flex gap-2">
          {(["boring", "icon", "letter"] as const).map((type) => (
            <button
              key={type}
              onClick={() => setProfile({ avatarType: type })}
              className={`rounded-lg border px-3 py-1.5 text-xs font-medium transition-colors ${
                profile.avatarType === type
                  ? "border-zinc-200 bg-zinc-200 text-zinc-800 dark:border-zinc-200 dark:bg-zinc-200 dark:text-zinc-800"
                  : "border-zinc-300 bg-white text-zinc-600 hover:bg-zinc-50 dark:border-zinc-600 dark:bg-zinc-800 dark:text-zinc-400 dark:hover:bg-zinc-700"
              }`}
            >
              {type === "boring" ? "Generated" : type === "icon" ? "Built-in Icon" : "Letter"}
            </button>
          ))}
        </div>
      </div>
      {/* Boring Avatars settings */}
      {profile.avatarType === "boring" && (
        <>
          <div className="space-y-1.5">
            <label className="block text-xs font-medium text-zinc-600 dark:text-zinc-400">
              Style
            </label>
            <div className="flex flex-wrap gap-2">
              {VARIANTS.map((v) => (
                <button
                  key={v.id}
                  onClick={() => setProfile({ avatarVariant: v.id })}
                  className={`rounded-lg border px-3 py-1.5 text-xs font-medium transition-colors ${
                    profile.avatarVariant === v.id
                      ? "border-zinc-200 bg-zinc-200 text-zinc-800 dark:border-zinc-200 dark:bg-zinc-200 dark:text-zinc-800"
                      : "border-zinc-300 bg-white text-zinc-600 hover:bg-zinc-50 dark:border-zinc-600 dark:bg-zinc-800 dark:text-zinc-400 dark:hover:bg-zinc-700"
                  }`}
                >
                  {v.label}
                </button>
              ))}
            </div>
          </div>

          <div className="space-y-1.5">
            <label className="block text-xs font-medium text-zinc-600 dark:text-zinc-400">
              Color Palette
            </label>
            <div className="flex flex-wrap gap-2">
              {PALETTES.map((p) => (
                <button
                  key={p.id}
                  onClick={() => setProfile({ colorPalette: p.id, avatarColors: [] })}
                  className={`rounded-lg border px-3 py-1.5 text-xs font-medium transition-colors ${
                    profile.colorPalette === p.id && profile.avatarColors.length === 0
                      ? "border-zinc-200 bg-zinc-200 text-zinc-800 dark:border-zinc-200 dark:bg-zinc-200 dark:text-zinc-800"
                      : "border-zinc-300 bg-white text-zinc-600 hover:bg-zinc-50 dark:border-zinc-600 dark:bg-zinc-800 dark:text-zinc-400 dark:hover:bg-zinc-700"
                  }`}
                >
                  <span className="flex items-center gap-1.5">
                    <span className="flex -space-x-1">
                      {COLOR_PALETTES[p.id]?.slice(0, 4).map((c, i) => (
                        <span
                          key={i}
                          className="inline-block h-3 w-3 rounded-full border border-white dark:border-zinc-700"
                          style={{ backgroundColor: c }}
                        />
                      ))}
                    </span>
                    {p.label}
                  </span>
                </button>
              ))}
            </div>
          </div>
        </>
      )}

      {/* Reset button */}
      <div className="border-t border-zinc-200 pt-4 dark:border-zinc-700">
        <button
          onClick={() => {
            resetProfile();
            setNameValue("我");
          }}
          className="rounded-lg btn-solid px-3 py-1.5 text-xs"
        >
          Reset to defaults
        </button>
      </div>
    </div>
  );
}