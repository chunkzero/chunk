/** The unescaped text of the first `<name>` element in `xml`, or undefined without one; enough for S3 and STS replies. */
export function xmlElement(xml: string, name: string): string | undefined {
  return new RegExp(`<${name}>([^<]*)</${name}>`)
    .exec(xml)?.[1]
    ?.replaceAll("&lt;", "<")
    .replaceAll("&gt;", ">")
    .replaceAll("&quot;", '"')
    .replaceAll("&apos;", "'")
    .replaceAll("&amp;", "&");
}
