export function greet(name: string): string {
  return `Hello, ${name}!`;
}

export function nowIso(): string {
  return new Date().toISOString();
}
