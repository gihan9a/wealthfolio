export interface TemplatePlaceholderValues {
  SYMBOL?: string;
  ISIN?: string;
  MIC?: string;
  CURRENCY?: string;
  currency?: string;
  TODAY?: string;
  FROM?: string;
  TO?: string;
}

const PLACEHOLDER_RE = /\{([A-Za-z]+)(?::([^}]+))?\}/g;

const DATE_PARTS: Record<string, (date: Date) => string> = {
  Y: (date) => String(date.getUTCFullYear()).padStart(4, "0"),
  m: (date) => String(date.getUTCMonth() + 1).padStart(2, "0"),
  d: (date) => String(date.getUTCDate()).padStart(2, "0"),
  s: (date) => String(Math.floor(date.getTime() / 1000)),
};

function formatStrftime(format: string, date: Date): string {
  return format.replace(/%([Ymds])/g, (match, token: string) => {
    return DATE_PARTS[token]?.(date) ?? match;
  });
}

function hasUnsupportedStrftimeDirective(format: string): boolean {
  const characters = format[Symbol.iterator]();
  let character = characters.next();
  while (!character.done) {
    if (character.value === "%") {
      const directive = characters.next();
      if (directive.done || !["Y", "m", "d", "s"].includes(directive.value)) return true;
    }
    character = characters.next();
  }
  return false;
}

function formatDate(value: string | undefined, format: string, now: Date): string | undefined {
  if (hasUnsupportedStrftimeDirective(format)) return undefined;
  const date = value ? new Date(`${value}T00:00:00Z`) : now;
  if (Number.isNaN(date.getTime())) return value;
  return formatStrftime(format, date);
}

export function resolveTemplatePlaceholder(
  token: string,
  format: string | undefined,
  values: TemplatePlaceholderValues,
  now = new Date(),
): string | undefined {
  const value = values[token as keyof TemplatePlaceholderValues];
  if (!format) return value;

  if (token === "DATE") return formatDate(undefined, format, now);
  if (token === "TODAY") return formatDate(values.TODAY, format, now);
  if (token === "FROM" || token === "TO") return formatDate(value, format, now);
  return undefined;
}

/**
 * Percent-encode a value for an `application/x-www-form-urlencoded` body the way
 * the backend does (space becomes `+`; only `A-Za-z0-9*-._` stay as they are).
 */
export function formEncode(value: string): string {
  return encodeURIComponent(value)
    .replace(/[!'()~]/g, (character) => `%${character.charCodeAt(0).toString(16).toUpperCase()}`)
    .replace(/%20/g, "+");
}

/** `escape` is applied to each substituted value, never to the template text. */
export function expandTemplatePlaceholders(
  template: string,
  values: TemplatePlaceholderValues,
  now = new Date(),
  escape?: (value: string) => string,
): string {
  return template.replace(PLACEHOLDER_RE, (match, token: string, format?: string) => {
    const resolved = resolveTemplatePlaceholder(token, format, values, now);
    if (resolved === undefined) return match;
    return escape ? escape(resolved) : resolved;
  });
}
