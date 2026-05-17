import { useState } from "react";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { UserAvatar, BUILTIN_ICONS, BUILTIN_ICON_IDS } from "../common/UserAvatar";

// ── Component ───────────────────────────────────────────────────────────

export function ProfileTab() {
  const { profile, setProfile, resetProfile } = useUserProfileStore();
  const [nameValue, setNameValue] = useState(profile.displayName);
  const [iconOpen, setIconOpen] = useState(false);

  return (
    <div className="max-w-lg space-y-4">
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
          className="w-full rounded-lg border border-zinc-300 bg-white px-3 py-2 text-xs text-zinc-800 placeholder:text-zinc-400 focus:border-zinc-400 focus:outline-none focus:ring-1 focus:ring-zinc-400 dark:border-zinc-600 dark:bg-zinc-800 dark:text-zinc-200 dark:placeholder:text-zinc-500"
        />
      </div>

      </div>

      <div className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
        <h2 className="mb-3 text-xs font-medium">Avatar Customization</h2>

        <div className="space-y-1.5">
        <label className="block text-xs font-medium text-zinc-600 dark:text-zinc-400">
          Avatar Type
        </label>
        <div className="flex gap-2">
          {(["icon", "letter"] as const).map((type) => (
            <button
              key={type}
              onClick={() => setProfile({ avatarType: type })}
              className={`rounded-lg border px-3 py-1.5 text-xs font-medium transition-colors ${
                profile.avatarType === type
                  ? "border-zinc-200 bg-zinc-200 text-zinc-800 dark:border-zinc-200 dark:bg-zinc-200 dark:text-zinc-800"
                  : "border-zinc-300 bg-white text-zinc-600 hover:bg-zinc-50 dark:border-zinc-600 dark:bg-zinc-800 dark:text-zinc-400 dark:hover:bg-zinc-700"
              }`}
            >
              {type === "icon" ? "Built-in Icon" : "Letter"}
            </button>
          ))}
        </div>
      </div>

      </div>

      <div className="rounded-lg border border-zinc-200 bg-white p-4 dark:border-zinc-700 dark:bg-zinc-800">
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