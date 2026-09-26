import { Link } from "@tanstack/react-router";

import { Page } from "../components/page.tsx";

export function Missing() {
  return (
    <Page>
      <p className="text-sm text-muted-foreground">
        Nothing here.{" "}
        <Link to="/" className="text-link underline-offset-4 hover:underline">
          Back to projects
        </Link>
      </p>
    </Page>
  );
}
