import { expect, test, type Page } from "@playwright/test";

// Roadmap P2 test plan: (first-launch setup or) login → create tenant → see key
// once → key absent after reload → rotate → freeze. Needs a running dev stack
// (Core admin bind + Vite) and ADMIN_PASSWORD set to the operator password (≥ 12
// chars — the setup screen enforces it on a fresh DB).
const PASSWORD = process.env.ADMIN_PASSWORD ?? "";

// Fresh DB → the panel shows the one-time setup screen; a reused DB → login.
// Handle whichever appears so the smoke passes on both.
async function authenticate(page: Page): Promise<void> {
  await page.goto("/");
  await expect(page.getByRole("heading", { name: "OPN Admin" })).toBeVisible();
  if (await page.getByLabel("New password").count()) {
    await page.getByLabel("New password").fill(PASSWORD);
    await page.getByLabel("Confirm password").fill(PASSWORD);
    await page.getByRole("button", { name: "Set password" }).click();
  } else {
    await page.getByLabel("Password", { exact: true }).fill(PASSWORD);
    await page.getByRole("button", { name: "Sign in" }).click();
  }
}

test("operator manages a tenant end to end", async ({ page }) => {
  test.skip(PASSWORD === "", "set ADMIN_PASSWORD to run the smoke");

  // First launch (set password) or sign in.
  await authenticate(page);

  // Create a uniquely-named tenant.
  const name = `pw-${Date.now()}`;
  await page.getByRole("button", { name: "+ Create tenant" }).click();
  await page.getByLabel("Name").fill(name);
  await page.getByRole("button", { name: "Create" }).click();

  // Key shown exactly once.
  const dialog = page.getByRole("dialog", { name: /API key/ });
  await expect(dialog).toBeVisible();
  const key = await dialog.locator("code").innerText();
  expect(key).toMatch(/^opn_/);
  await dialog.getByRole("button", { name: "I saved it" }).click();

  // New tenant is listed.
  const row = page.locator("tr", { hasText: name });
  await expect(row).toBeVisible();

  // Key absent after reload (in-memory only) — reload bounces to login.
  await page.reload();
  await expect(page.getByRole("button", { name: "Sign in" })).toBeVisible();
  expect(await page.content()).not.toContain(key);

  // Re-login (password is set now, so always the login screen), rotate, freeze.
  await page.getByLabel("Password", { exact: true }).fill(PASSWORD);
  await page.getByRole("button", { name: "Sign in" }).click();
  const row2 = page.locator("tr", { hasText: name });
  await row2.getByRole("button", { name: "Rotate key" }).click();
  await page.getByRole("dialog", { name: "Rotate API key" }).getByRole("button", { name: "Rotate key" }).click();
  await expect(page.getByRole("dialog", { name: /API key/ })).toBeVisible();
  await page.getByRole("button", { name: "I saved it" }).click();

  await page.locator("tr", { hasText: name }).getByRole("button", { name: "Freeze" }).click();
  await expect(page.locator("tr", { hasText: name }).getByText("frozen")).toBeVisible();
});
