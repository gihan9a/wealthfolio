import { useTranslation } from "react-i18next";

import { cn } from "@/lib/utils";
import { Alert, AlertDescription } from "@wealthfolio/ui/components/ui/alert";
import { Icons } from "@wealthfolio/ui/components/ui/icons";

/** Providers whose data source needs a notice before the user turns them on. */
const DISCLAIMED_PROVIDERS = new Set(["CSE_LK"]);

/**
 * The notice a provider shows wherever it can be turned on, or nothing.
 * Informational only: nothing is stored when it is seen.
 */
export function ProviderDisclaimer({
  providerId,
  className,
}: {
  providerId: string;
  className?: string;
}) {
  const { t } = useTranslation();
  if (!DISCLAIMED_PROVIDERS.has(providerId)) return null;
  return (
    <Alert variant="warning" className={cn("px-3 py-2.5", className)}>
      <Icons.AlertTriangle className="h-4 w-4" />
      <AlertDescription className="text-xs">
        {t(`settings:market_data_page.providerDisclaimers.${providerId}`)}
      </AlertDescription>
    </Alert>
  );
}
