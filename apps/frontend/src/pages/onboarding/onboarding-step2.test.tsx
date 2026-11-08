import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import { SUPPORTED_LOCALES } from "@/i18n/locales";
import { detectDefaultCurrency, OnboardingStep2 } from "./onboarding-step2";

const mocks = vi.hoisted(() => ({
  settings: { language: "en" } as { language: string },
  updateSettings: vi.fn(() => Promise.resolve()),
}));

vi.mock("@/lib/settings-provider", () => ({
  useSettingsContext: () => ({
    settings: mocks.settings,
    updateSettings: mocks.updateSettings,
  }),
}));

function renderStep2(language = "en") {
  mocks.settings = { language };
  return render(<OnboardingStep2 onNext={vi.fn()} onValidityChange={vi.fn()} />);
}

describe("OnboardingStep2 language picker", () => {
  it("shows only the popular languages as chips", () => {
    renderStep2();

    for (const code of ["en", "fr", "de", "es", "zh", "ja", "ko"]) {
      expect(screen.getByTestId(`language-${code}-button`)).toBeInTheDocument();
    }
    // Everything else lives behind the "Other" chip.
    expect(screen.queryByTestId("language-pt-button")).not.toBeInTheDocument();
    expect(screen.queryByTestId("language-it-button")).not.toBeInTheDocument();
  });

  it("lists every supported locale in the overlay", async () => {
    const user = userEvent.setup();
    renderStep2();

    await user.click(screen.getAllByRole("button", { name: /other/i })[0]);

    for (const locale of SUPPORTED_LOCALES) {
      expect(screen.getAllByTestId(`language-${locale.code}-button`).length).toBeGreaterThan(0);
    }
    // A popular language now appears both as a chip and as an overlay row.
    expect(screen.getAllByTestId("language-en-button")).toHaveLength(2);
  });

  it("filters the overlay and reports when nothing matches", async () => {
    const user = userEvent.setup();
    renderStep2();

    await user.click(screen.getAllByRole("button", { name: /other/i })[0]);
    const search = screen.getByPlaceholderText("Search languages...");

    await user.type(search, "portug");
    expect(screen.getByTestId("language-pt-button")).toBeInTheDocument();
    expect(screen.queryByTestId("language-it-button")).not.toBeInTheDocument();

    await user.clear(search);
    await user.type(search, "zzzz");
    expect(screen.getByText("No languages found")).toBeInTheDocument();
  });

  it("substitutes the selected language into the chips when it is not popular", () => {
    renderStep2("pt");

    expect(screen.getByTestId("language-pt-button")).toBeInTheDocument();
    // It takes the last popular slot rather than growing the row.
    expect(screen.queryByTestId("language-ko-button")).not.toBeInTheDocument();
    expect(screen.getByTestId("language-en-button")).toBeInTheDocument();
  });
});

describe("detectDefaultCurrency for Sri Lanka", () => {
  it.each(["en-LK", "si-LK", "ta-LK", "en_LK", "si-lk"])("suggests LKR for %s", (tag) => {
    expect(detectDefaultCurrency(tag)).toBe("LKR");
  });

  it("does not change suggestions for other locales", () => {
    expect(detectDefaultCurrency("en-GB")).toBe("GBP");
    expect(detectDefaultCurrency("en-US")).toBe("USD");
    expect(detectDefaultCurrency("en")).toBeUndefined();
    expect(detectDefaultCurrency("sv")).toBe("EUR");
  });
});

describe("OnboardingStep2 region currency suggestion", () => {
  async function pickRegion(user: ReturnType<typeof userEvent.setup>, name: RegExp) {
    const regionCard = screen.getByTestId("onboarding-formatting-locale");
    await user.click(within(regionCard).getByRole("button", { name: /other/i }));
    await user.click(screen.getByRole("button", { name }));
  }

  it("suggests LKR when Sri Lanka is chosen", async () => {
    const user = userEvent.setup();
    renderStep2();

    await pickRegion(user, /sri lanka/i);

    expect(screen.getByTestId("currency-lkr-button")).toBeInTheDocument();
    expect(mocks.updateSettings).toHaveBeenCalledWith({ formattingRegion: "LK" });
  });

  it("keeps suggesting the region's usual currency for other regions", async () => {
    const user = userEvent.setup();
    renderStep2();

    await pickRegion(user, /japan/i);

    expect(screen.getByTestId("currency-jpy-button")).toBeInTheDocument();
    expect(screen.queryByTestId("currency-lkr-button")).not.toBeInTheDocument();
  });

  it("does not override a currency the user already picked", async () => {
    const user = userEvent.setup();
    renderStep2();

    await user.click(await screen.findByTestId("currency-eur-button"));
    await pickRegion(user, /sri lanka/i);

    expect(screen.queryByTestId("currency-lkr-button")).not.toBeInTheDocument();
    expect(screen.getByTestId("currency-eur-button")).toHaveClass("border-primary");
  });
});
