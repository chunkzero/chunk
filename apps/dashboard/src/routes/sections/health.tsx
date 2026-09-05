import { Page } from "@/components/page";
import { Logs } from "./logs";

export function Server() {
    return (
        <Page>
            <h1 className="text-xl font-semibold">Server logs</h1>
            <Logs />
        </Page>
    );
}
