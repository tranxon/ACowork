import { type ReactNode } from "react";

interface RadioOption<T> {
    label: ReactNode;
    value: T;
}

interface RadioGroupProps<T> {
    name: string;
    value: T;
    options: RadioOption<T>[];
    onChange: (value: T) => void;
}

export function RadioGroup<T>({
    name,
    value,
    options,
    onChange,
}: RadioGroupProps<T>) {
    return (
        <div className="flex flex-wrap gap-[var(--ui-radio-gap)]">
            {options.map((opt) => (
                <label
                    key={String(opt.value)}
                    className="flex cursor-pointer items-center gap-[var(--ui-radio-label-gap)] text-xs"
                >
                    <span className="relative flex items-center justify-center">
                        <input
                            type="radio"
                            name={name}
                            value={String(opt.value)}
                            checked={value === opt.value}
                            onChange={() => onChange(opt.value)}
                            className="peer sr-only"
                        />
                        <span className="block h-[var(--ui-radio-size)] w-[var(--ui-radio-size)] rounded-full bg-[var(--ui-radio-bg)] peer-checked:bg-[var(--color-accent)]" />
                    </span>
                    {opt.label}
                </label>
            ))}
        </div>
    );
}
