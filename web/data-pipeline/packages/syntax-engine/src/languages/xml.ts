import type {
  LanguageDefinition,
  LineDiagnostic,
  LineToken,
  SyntaxDiagnostic,
  TokenKind,
} from "../types.js";

type XmlMode = "text" | "tag" | "comment" | "cdata" | "processing" | "doctype" | "attribute";

interface XmlStackNode {
  readonly name: string;
  readonly parent: XmlStackNode | null;
  readonly depth: number;
  readonly hash: number;
}

export interface XmlLexerState {
  readonly mode: XmlMode;
  readonly quote: "" | "'" | '"';
  readonly tagName: string;
  readonly closingTag: boolean;
  readonly stack: XmlStackNode | null;
  readonly overflowDepth: number;
  readonly doctypeDepth: number;
  readonly doctypeQuote: "" | "'" | '"';
  readonly doctypeComment: boolean;
  readonly doctypeProcessing: boolean;
}

const MAX_LEXICAL_DEPTH = 2048;
const MAX_VALIDATION_DEPTH = 8192;
// XML 1.0 (Fifth Edition) NameStartChar / NameChar ranges.
const XML_NAME = /^[:A-Z_a-z\u00c0-\u00d6\u00d8-\u00f6\u00f8-\u02ff\u0370-\u037d\u037f-\u1fff\u200c-\u200d\u2070-\u218f\u2c00-\u2fef\u3001-\ud7ff\uf900-\ufdcf\ufdf0-\ufffd\u{10000}-\u{effff}][:A-Z_a-z\u00c0-\u00d6\u00d8-\u00f6\u00f8-\u02ff\u0370-\u037d\u037f-\u1fff\u200c-\u200d\u2070-\u218f\u2c00-\u2fef\u3001-\ud7ff\uf900-\ufdcf\ufdf0-\ufffd\u{10000}-\u{effff}0-9.\-\u00b7\u0300-\u036f\u203f-\u2040]*/u;
const ENTITY = /^&(?:#\d+|#x[\da-fA-F]+|[:A-Z_a-z\u00c0-\u00d6\u00d8-\u00f6\u00f8-\u02ff\u0370-\u037d\u037f-\u1fff\u200c-\u200d\u2070-\u218f\u2c00-\u2fef\u3001-\ud7ff\uf900-\ufdcf\ufdf0-\ufffd\u{10000}-\u{effff}][:A-Z_a-z\u00c0-\u00d6\u00d8-\u00f6\u00f8-\u02ff\u0370-\u037d\u037f-\u1fff\u200c-\u200d\u2070-\u218f\u2c00-\u2fef\u3001-\ud7ff\uf900-\ufdcf\ufdf0-\ufffd\u{10000}-\u{effff}0-9.\-\u00b7\u0300-\u036f\u203f-\u2040]*);/u;
const PREDEFINED_ENTITIES = new Set(["amp", "lt", "gt", "apos", "quot"]);
const XML_DECLARATION = /^<\?xml[ \t\r\n]+version[ \t\r\n]*=[ \t\r\n]*(?:"1\.0"|'1\.0')(?:[ \t\r\n]+encoding[ \t\r\n]*=[ \t\r\n]*(?:"[A-Za-z][A-Za-z0-9._-]*"|'[A-Za-z][A-Za-z0-9._-]*'))?(?:[ \t\r\n]+standalone[ \t\r\n]*=[ \t\r\n]*(?:"(?:yes|no)"|'(?:yes|no)'))?[ \t\r\n]*\?>$/u;

function state(overrides: Partial<XmlLexerState> = {}): XmlLexerState {
  return Object.freeze({
    mode: "text",
    quote: "",
    tagName: "",
    closingTag: false,
    stack: null,
    overflowDepth: 0,
    doctypeDepth: 0,
    doctypeQuote: "",
    doctypeComment: false,
    doctypeProcessing: false,
    ...overrides,
  });
}

function tagHash(parent: XmlStackNode | null, name: string): number {
  let hash = parent?.hash ?? 2166136261;
  for (let index = 0; index < name.length; index += 1) {
    hash = Math.imul(hash ^ name.charCodeAt(index), 16777619);
  }
  return hash >>> 0;
}

function pushTag(parent: XmlStackNode | null, name: string): XmlStackNode {
  return Object.freeze({
    name,
    parent,
    depth: (parent?.depth ?? 0) + 1,
    hash: tagHash(parent, name),
  });
}

function xmlStatesEqual(left: Readonly<XmlLexerState>, right: Readonly<XmlLexerState>): boolean {
  if (left === right) return true;
  if (
    left.mode !== right.mode
    || left.quote !== right.quote
    || left.tagName !== right.tagName
    || left.closingTag !== right.closingTag
    || left.overflowDepth !== right.overflowDepth
    || left.doctypeDepth !== right.doctypeDepth
    || left.doctypeQuote !== right.doctypeQuote
    || left.doctypeComment !== right.doctypeComment
    || left.doctypeProcessing !== right.doctypeProcessing
  ) return false;
  let leftNode = left.stack;
  let rightNode = right.stack;
  if (leftNode === rightNode) return true;
  if (leftNode?.depth !== rightNode?.depth || leftNode?.hash !== rightNode?.hash) return false;
  while (leftNode && rightNode) {
    if (leftNode === rightNode) return true;
    if (leftNode.name !== rightNode.name) return false;
    leftNode = leftNode.parent;
    rightNode = rightNode.parent;
  }
  return leftNode === rightNode;
}

function continueUntil(
  line: string,
  close: string,
  kind: TokenKind,
  current: XmlLexerState,
  nextMode: XmlMode,
) {
  const end = line.indexOf(close);
  if (end < 0) {
    return { tokens: line.length ? [{ from: 0, to: line.length, kind }] : [], state: current };
  }
  return {
    tokens: [{ from: 0, to: end + close.length, kind }],
    state: state({ ...current, mode: nextMode }),
    index: end + close.length,
  };
}

function scanDoctype(
  source: string,
  from: number,
  startDepth = 0,
  startQuote: "" | "'" | '"' = "",
  startComment = false,
  startProcessing = false,
): {
  readonly end: number;
  readonly closed: boolean;
  readonly depth: number;
  readonly quote: "" | "'" | '"';
  readonly comment: boolean;
  readonly processing: boolean;
} {
  let depth = startDepth;
  let quote = startQuote;
  let comment = startComment;
  let processing = startProcessing;
  let index = from;
  while (index < source.length) {
    const character = source[index];
    if (comment) {
      if (source.startsWith("-->", index)) {
        comment = false;
        index += 3;
        continue;
      }
    } else if (processing) {
      if (source.startsWith("?>", index)) {
        processing = false;
        index += 2;
        continue;
      }
    } else if (quote) {
      if (character === quote) quote = "";
    } else if (source.startsWith("<!--", index)) {
      comment = true;
      index += 4;
      continue;
    } else if (source.startsWith("<?", index)) {
      processing = true;
      index += 2;
      continue;
    } else if (character === '"' || character === "'") {
      quote = character;
    } else if (character === "[") {
      depth += 1;
    } else if (character === "]") {
      depth = Math.max(0, depth - 1);
    } else if (character === ">" && depth === 0) {
      return { end: index + 1, closed: true, depth, quote, comment, processing };
    }
    index += 1;
  }
  return { end: source.length, closed: false, depth, quote, comment, processing };
}

function xmlError(from: number, to: number, message: string, code: string): readonly SyntaxDiagnostic[] {
  return [{ from, to: Math.max(from, to), severity: "error", message, code }];
}

function isXmlCharacter(codePoint: number): boolean {
  return codePoint === 0x9
    || codePoint === 0xa
    || codePoint === 0xd
    || (codePoint >= 0x20 && codePoint <= 0xd7ff)
    || (codePoint >= 0xe000 && codePoint <= 0xfffd)
    || (codePoint >= 0x10000 && codePoint <= 0x10ffff);
}

interface XmlEntityReference {
  readonly from: number;
  readonly to: number;
  readonly name: string;
}

function scanEntityReferences(
  source: string,
  from: number,
  to: number,
): { references: XmlEntityReference[]; diagnostics: readonly SyntaxDiagnostic[] } {
  const references: XmlEntityReference[] = [];
  let index = source.indexOf("&", from);
  while (index >= 0 && index < to) {
    const entity = source.slice(index, to).match(ENTITY)?.[0];
    if (!entity) {
      return {
        references,
        diagnostics: xmlError(index, Math.min(to, index + 1), "Некорректная XML-сущность", "xml-invalid-entity"),
      };
    }
    const body = entity.slice(1, -1);
    if (body[0] === "#") {
      const hexadecimal = body[1] === "x";
      const value = Number.parseInt(body.slice(hexadecimal ? 2 : 1), hexadecimal ? 16 : 10);
      if (!Number.isSafeInteger(value) || !isXmlCharacter(value)) {
        return {
          references,
          diagnostics: xmlError(
            index,
            index + entity.length,
            "Ссылка указывает на недопустимый XML-символ",
            "xml-invalid-character-reference",
          ),
        };
      }
    } else {
      references.push({ from: index, to: index + entity.length, name: body });
    }
    index = source.indexOf("&", index + entity.length);
  }
  return { references, diagnostics: [] };
}

function resolveEntityReferences(
  references: readonly XmlEntityReference[],
  declared: ReadonlySet<string>,
  unparsed: ReadonlySet<string>,
): readonly SyntaxDiagnostic[] {
  for (const reference of references) {
    if (PREDEFINED_ENTITIES.has(reference.name)) continue;
    if (unparsed.has(reference.name)) {
      return xmlError(
        reference.from,
        reference.to,
        `Unparsed XML-сущность &${reference.name}; нельзя использовать как entity reference`,
        "xml-unparsed-entity-reference",
      );
    }
    if (!declared.has(reference.name)) {
      return xmlError(
        reference.from,
        reference.to,
        `XML-сущность &${reference.name}; не объявлена`,
        "xml-undeclared-entity",
      );
    }
  }
  return [];
}

function checkEntities(
  source: string,
  from: number,
  to: number,
  declared: ReadonlySet<string>,
  unparsed: ReadonlySet<string>,
): readonly SyntaxDiagnostic[] {
  const scanned = scanEntityReferences(source, from, to);
  return scanned.diagnostics.length
    ? scanned.diagnostics
    : resolveEntityReferences(scanned.references, declared, unparsed);
}

function checkParameterEntityReferences(
  source: string,
  from: number,
  to: number,
  declared: ReadonlySet<string>,
): readonly SyntaxDiagnostic[] {
  let index = source.indexOf("%", from);
  while (index >= 0 && index < to) {
    const name = source.slice(index + 1, to).match(XML_NAME)?.[0] ?? "";
    const end = index + name.length + 1;
    if (!name || source[end] !== ";") {
      return xmlError(
        index,
        Math.min(to, Math.max(index + 1, end + 1)),
        "Некорректная parameter entity reference",
        "xml-invalid-parameter-entity-reference",
      );
    }
    if (!declared.has(name)) {
      return xmlError(
        index,
        end + 1,
        `Parameter entity %${name}; не объявлена до ссылки`,
        "xml-undeclared-parameter-entity",
      );
    }
    index = source.indexOf("%", end + 1);
  }
  return [];
}

function skipXmlWhitespace(source: string, from: number, to = source.length): number {
  let index = from;
  while (index < to && /[\u0020\t\r\n]/u.test(source[index])) index += 1;
  return index;
}

function quotedLiteralEnd(source: string, from: number, to: number): number {
  const quote = source[from];
  if (quote !== '"' && quote !== "'") return -1;
  const end = source.indexOf(quote, from + 1);
  return end >= 0 && end < to ? end + 1 : -1;
}

function markupDeclarationEnd(source: string, from: number, to: number): number {
  let quote = "";
  for (let index = from; index < to; index += 1) {
    const character = source[index];
    if (quote) {
      if (character === quote) quote = "";
    } else if (character === '"' || character === "'") quote = character;
    else if (character === ">") return index + 1;
  }
  return -1;
}

function validParenthesizedModel(value: string): boolean {
  let index = 0;
  const maxDepth = 512;

  const skipWhitespace = () => {
    while (index < value.length && /[\u0020\t\r\n]/u.test(value[index])) index += 1;
  };
  const readName = () => {
    const name = value.slice(index).match(XML_NAME)?.[0] ?? "";
    index += name.length;
    return name.length > 0;
  };
  const readOccurrence = () => {
    if (value[index] === "?" || value[index] === "*" || value[index] === "+") index += 1;
  };

  function parseParticle(depth: number): boolean {
    if (depth > maxDepth) return false;
    const valid = value[index] === "(" ? parseChildrenGroup(depth + 1) : readName();
    if (!valid) return false;
    readOccurrence();
    return true;
  }

  function parseChildrenGroup(depth: number): boolean {
    if (depth > maxDepth || value[index] !== "(") return false;
    index += 1;
    skipWhitespace();
    if (!parseParticle(depth)) return false;
    skipWhitespace();

    let separator = "";
    while (value[index] === "|" || value[index] === ",") {
      if (separator && separator !== value[index]) return false;
      separator = value[index];
      index += 1;
      skipWhitespace();
      if (!parseParticle(depth)) return false;
      skipWhitespace();
    }
    if (value[index] !== ")") return false;
    index += 1;
    return true;
  }

  function parseMixed(): boolean {
    index = 1;
    skipWhitespace();
    if (!value.startsWith("#PCDATA", index)) return false;
    index += 7;
    skipWhitespace();

    const names = new Set<string>();
    while (value[index] === "|") {
      index += 1;
      skipWhitespace();
      const nameAt = index;
      if (!readName()) return false;
      const name = value.slice(nameAt, index);
      if (names.has(name)) return false;
      names.add(name);
      skipWhitespace();
    }
    if (value[index] !== ")") return false;
    index += 1;
    if (value[index] === "*") index += 1;
    else if (names.size) return false;
    return index === value.length;
  }

  if (value[index] !== "(") return false;
  index += 1;
  skipWhitespace();
  if (value.startsWith("#PCDATA", index)) return parseMixed();

  index = 0;
  if (!parseChildrenGroup(0)) return false;
  readOccurrence();
  return index === value.length;
}

function enumerationEnd(source: string, from: number, to: number, namesOnly: boolean): number {
  if (source[from] !== "(") return -1;
  let index = skipXmlWhitespace(source, from + 1, to);
  const seen = new Set<string>();
  while (index < to) {
    let token = "";
    if (namesOnly) token = source.slice(index, to).match(XML_NAME)?.[0] ?? "";
    else {
      const prefixed = `_${source.slice(index, to)}`.match(XML_NAME)?.[0] ?? "";
      token = prefixed.slice(1);
    }
    if (!token || seen.has(token)) return -1;
    seen.add(token);
    index = skipXmlWhitespace(source, index + token.length, to);
    if (source[index] === ")") return index + 1;
    if (source[index] !== "|") return -1;
    index = skipXmlWhitespace(source, index + 1, to);
  }
  return -1;
}

function validateAttlist(
  source: string,
  from: number,
  to: number,
  declaredEntities: ReadonlySet<string>,
  unparsedEntities: ReadonlySet<string>,
): readonly SyntaxDiagnostic[] | null {
  let index = from;
  const simpleTypes = [
    "CDATA", "IDREFS", "IDREF", "ID", "ENTITIES", "ENTITY", "NMTOKENS", "NMTOKEN",
  ];
  while ((index = skipXmlWhitespace(source, index, to)) < to) {
    const attribute = source.slice(index).match(XML_NAME)?.[0] ?? "";
    if (!attribute) return null;
    index += attribute.length;
    if (!/[\u0020\t\r\n]/u.test(source[index] ?? "")) return null;
    index = skipXmlWhitespace(source, index, to);

    const simpleType = simpleTypes.find((candidate) => (
      source.startsWith(candidate, index)
      && /[\u0020\t\r\n]/u.test(source[index + candidate.length] ?? "")
    ));
    if (simpleType) index += simpleType.length;
    else if (source.startsWith("NOTATION", index)) {
      index += 8;
      if (!/[\u0020\t\r\n]/u.test(source[index] ?? "")) return null;
      index = skipXmlWhitespace(source, index, to);
      index = enumerationEnd(source, index, to, true);
      if (index < 0) return null;
    } else {
      index = enumerationEnd(source, index, to, false);
      if (index < 0) return null;
    }

    if (!/[\u0020\t\r\n]/u.test(source[index] ?? "")) return null;
    index = skipXmlWhitespace(source, index, to);
    if (source.startsWith("#REQUIRED", index)) index += 9;
    else if (source.startsWith("#IMPLIED", index)) index += 8;
    else {
      if (source.startsWith("#FIXED", index)) {
        index += 6;
        if (!/[\u0020\t\r\n]/u.test(source[index] ?? "")) return null;
        index = skipXmlWhitespace(source, index, to);
      }
      const valueEnd = quotedLiteralEnd(source, index, to);
      if (valueEnd < 0) return null;
      const valueFrom = index + 1;
      const valueTo = valueEnd - 1;
      const lessThan = source.indexOf("<", valueFrom);
      if (lessThan >= 0 && lessThan < valueTo) {
        return xmlError(
          lessThan,
          lessThan + 1,
          "Символ < недопустим в значении XML-атрибута",
          "xml-invalid-attribute-value",
        );
      }
      const entityDiagnostics = checkEntities(
        source,
        valueFrom,
        valueTo,
        declaredEntities,
        unparsedEntities,
      );
      if (entityDiagnostics.length) return entityDiagnostics;
      index = valueEnd;
    }
    if (index < to && !/[\u0020\t\r\n]/u.test(source[index])) return null;
  }
  return [];
}

function validateInternalSubset(
  source: string,
  from: number,
  to: number,
  declaredEntities: Set<string>,
  unparsedEntities: Set<string>,
  declaredParameterEntities: Set<string>,
): readonly SyntaxDiagnostic[] {
  let index = from;
  const deferredEntityReferences: XmlEntityReference[] = [];
  const fail = (start: number, end: number, message: string) => xmlError(
    start,
    Math.max(start + 1, end),
    message,
    "xml-invalid-doctype-subset",
  );
  const requireSpace = (at: number) => /[\u0020\t\r\n]/u.test(source[at] ?? "");

  while ((index = skipXmlWhitespace(source, index, to)) < to) {
    if (source.startsWith("<!--", index)) {
      const end = source.indexOf("-->", index + 4);
      if (end < 0 || end + 3 > to) return fail(index, to, "Комментарий в DOCTYPE не закрыт");
      const inner = source.slice(index + 4, end);
      if (inner.includes("--") || inner.endsWith("-")) return fail(index, end + 3, "Некорректный комментарий в DOCTYPE");
      index = end + 3;
      continue;
    }
    if (source.startsWith("<?", index)) {
      const end = source.indexOf("?>", index + 2);
      if (end < 0 || end + 2 > to) return fail(index, to, "Processing instruction в DOCTYPE не закрыта");
      const target = source.slice(index + 2, end).match(XML_NAME)?.[0] ?? "";
      if (!target || target.toLowerCase() === "xml") return fail(index, end + 2, "Некорректная processing instruction в DOCTYPE");
      index = end + 2;
      continue;
    }
    if (source[index] === "%") {
      const name = source.slice(index + 1).match(XML_NAME)?.[0] ?? "";
      const end = index + 1 + name.length;
      if (!name || source[end] !== ";") return fail(index, Math.min(to, end + 1), "Некорректная parameter entity reference");
      if (!declaredParameterEntities.has(name)) {
        return xmlError(
          index,
          end + 1,
          `Parameter entity %${name}; не объявлена до ссылки`,
          "xml-undeclared-parameter-entity",
        );
      }
      index = end + 1;
      continue;
    }

    const declarationEnd = markupDeclarationEnd(source, index, to);
    if (declarationEnd < 0) return fail(index, to, "Markup declaration в DOCTYPE не закрыта");
    const keyword = ["ELEMENT", "ATTLIST", "ENTITY", "NOTATION"]
      .find((candidate) => source.startsWith(`<!${candidate}`, index));
    if (!keyword) return fail(index, declarationEnd, "Неизвестная декларация во внутреннем DTD subset");

    let cursor = index + keyword.length + 2;
    if (!requireSpace(cursor)) return fail(index, declarationEnd, `После ${keyword} требуется пробел`);
    cursor = skipXmlWhitespace(source, cursor, declarationEnd - 1);
    let parameterEntity = false;
    if (keyword === "ENTITY" && source[cursor] === "%") {
      parameterEntity = true;
      cursor += 1;
      if (!requireSpace(cursor)) return fail(index, declarationEnd, "После % требуется пробел");
      cursor = skipXmlWhitespace(source, cursor, declarationEnd - 1);
    }
    const name = source.slice(cursor).match(XML_NAME)?.[0] ?? "";
    if (!name) return fail(cursor, declarationEnd, `В ${keyword} отсутствует XML-имя`);
    cursor += name.length;
    const hadSpace = requireSpace(cursor);
    cursor = skipXmlWhitespace(source, cursor, declarationEnd - 1);
    const contentEnd = declarationEnd - 1;
    let trimmedContentEnd = contentEnd;
    while (trimmedContentEnd > cursor && /[\u0020\t\r\n]/u.test(source[trimmedContentEnd - 1])) {
      trimmedContentEnd -= 1;
    }
    const content = source.slice(cursor, trimmedContentEnd);

    if (keyword === "ELEMENT") {
      if (!hadSpace || !(content === "EMPTY" || content === "ANY" || validParenthesizedModel(content))) {
        return fail(index, declarationEnd, "Некорректная ELEMENT declaration");
      }
    } else if (keyword === "ATTLIST") {
      const attlistDiagnostics = validateAttlist(
        source,
        cursor,
        contentEnd,
        declaredEntities,
        unparsedEntities,
      );
      if (attlistDiagnostics === null) {
        return fail(index, declarationEnd, "Некорректная ATTLIST declaration");
      }
      if (attlistDiagnostics.length) return attlistDiagnostics;
    } else if (keyword === "ENTITY") {
      if (!hadSpace || !content) return fail(index, declarationEnd, "У ENTITY отсутствует значение");
      let definitionAt = cursor;
      let definitionEnd = quotedLiteralEnd(source, definitionAt, contentEnd);
      let external = false;
      if (definitionEnd < 0 && source.startsWith("SYSTEM", definitionAt)) {
        external = true;
        if (!requireSpace(definitionAt + 6)) return fail(index, declarationEnd, "После SYSTEM требуется quoted literal");
        definitionAt = skipXmlWhitespace(source, definitionAt + 6, contentEnd);
        definitionEnd = quotedLiteralEnd(source, definitionAt, contentEnd);
      } else if (definitionEnd < 0 && source.startsWith("PUBLIC", definitionAt)) {
        external = true;
        if (!requireSpace(definitionAt + 6)) return fail(index, declarationEnd, "После PUBLIC требуется quoted literal");
        definitionAt = skipXmlWhitespace(source, definitionAt + 6, contentEnd);
        const publicEnd = quotedLiteralEnd(source, definitionAt, contentEnd);
        definitionAt = publicEnd < 0 ? -1 : skipXmlWhitespace(source, publicEnd, contentEnd);
        definitionEnd = definitionAt < 0 ? -1 : quotedLiteralEnd(source, definitionAt, contentEnd);
      }
      if (definitionEnd < 0) return fail(index, declarationEnd, "Некорректное значение ENTITY");
      if (!external) {
        const scanned = scanEntityReferences(source, definitionAt + 1, definitionEnd - 1);
        if (scanned.diagnostics.length) return scanned.diagnostics;
        deferredEntityReferences.push(...scanned.references);
        const parameterDiagnostics = checkParameterEntityReferences(
          source,
          definitionAt + 1,
          definitionEnd - 1,
          declaredParameterEntities,
        );
        if (parameterDiagnostics.length) return parameterDiagnostics;
      }
      let remainderAt = skipXmlWhitespace(source, definitionEnd, contentEnd);
      let unparsed = false;
      if (!parameterEntity && source.startsWith("NDATA", remainderAt)) {
        if (!external) return fail(index, declarationEnd, "NDATA допустим только у external entity");
        if (!requireSpace(remainderAt + 5)) return fail(index, declarationEnd, "После NDATA требуется notation name");
        remainderAt = skipXmlWhitespace(source, remainderAt + 5, contentEnd);
        const notation = source.slice(remainderAt).match(XML_NAME)?.[0] ?? "";
        remainderAt += notation.length;
        if (!notation) return fail(index, declarationEnd, "После NDATA требуется notation name");
        unparsed = true;
      }
      if (skipXmlWhitespace(source, remainderAt, contentEnd) !== contentEnd) {
        return fail(index, declarationEnd, "Лишнее содержимое в ENTITY declaration");
      }
      if (parameterEntity) {
        declaredParameterEntities.add(name);
      } else if (!declaredEntities.has(name) && !unparsedEntities.has(name)) {
        (unparsed ? unparsedEntities : declaredEntities).add(name);
      }
    } else {
      const system = content.startsWith("SYSTEM");
      const publicId = content.startsWith("PUBLIC");
      if (!hadSpace || (!system && !publicId)) {
        return fail(index, declarationEnd, "Некорректная NOTATION declaration");
      }
      let literalAt = cursor + (system ? 6 : 6);
      if (!requireSpace(literalAt)) return fail(index, declarationEnd, "После NOTATION external ID требуется пробел");
      literalAt = skipXmlWhitespace(source, literalAt, contentEnd);
      let literalEnd = quotedLiteralEnd(source, literalAt, contentEnd);
      if (literalEnd < 0) return fail(index, declarationEnd, "NOTATION identifier должен быть в кавычках");
      literalEnd = skipXmlWhitespace(source, literalEnd, contentEnd);
      if (publicId && literalEnd < contentEnd) {
        literalEnd = quotedLiteralEnd(source, literalEnd, contentEnd);
        if (literalEnd < 0) return fail(index, declarationEnd, "Некорректный NOTATION system literal");
        literalEnd = skipXmlWhitespace(source, literalEnd, contentEnd);
      }
      if (literalEnd !== contentEnd) return fail(index, declarationEnd, "Лишнее содержимое в NOTATION declaration");
    }
    index = declarationEnd;
  }
  return resolveEntityReferences(deferredEntityReferences, declaredEntities, unparsedEntities);
}

function validateDoctypeBody(
  source: string,
  from: number,
  to: number,
  declaredEntities: Set<string>,
  unparsedEntities: Set<string>,
  declaredParameterEntities: Set<string>,
): readonly SyntaxDiagnostic[] {
  let index = skipXmlWhitespace(source, from, to);
  const invalid = (at: number, message: string) => xmlError(
    at,
    Math.max(at + 1, to),
    message,
    "xml-invalid-doctype",
  );

  if (source.startsWith("SYSTEM", index) || source.startsWith("PUBLIC", index)) {
    const isPublic = source.startsWith("PUBLIC", index);
    index += 6;
    if (!/[\u0020\t\r\n]/u.test(source[index] ?? "")) return invalid(index, "После external ID требуется quoted literal");
    index = skipXmlWhitespace(source, index, to);
    let literalEnd = quotedLiteralEnd(source, index, to);
    if (literalEnd < 0) return invalid(index, "External ID должен быть в кавычках");
    index = skipXmlWhitespace(source, literalEnd, to);
    if (isPublic) {
      literalEnd = quotedLiteralEnd(source, index, to);
      if (literalEnd < 0) return invalid(index, "После PUBLIC identifier требуется system literal");
      index = skipXmlWhitespace(source, literalEnd, to);
    }
  }

  if (source[index] === "[") {
    let close = to - 1;
    while (close > index && /[\u0020\t\r\n]/u.test(source[close])) close -= 1;
    if (source[close] !== "]") return invalid(index, "Внутренний DTD subset не закрыт");
    const subsetDiagnostics = validateInternalSubset(
      source,
      index + 1,
      close,
      declaredEntities,
      unparsedEntities,
      declaredParameterEntities,
    );
    if (subsetDiagnostics.length) return subsetDiagnostics;
    index = skipXmlWhitespace(source, close + 1, to);
  }

  return index === to ? [] : invalid(index, "Некорректное содержимое DOCTYPE");
}

function validateXml(source: string): readonly SyntaxDiagnostic[] {
  const stack: string[] = [];
  for (let offset = 0; offset < source.length;) {
    const codePoint = source.codePointAt(offset) ?? 0;
    const width = codePoint > 0xffff ? 2 : 1;
    if (!isXmlCharacter(codePoint)) {
      return xmlError(offset, offset + width, "Недопустимый XML-символ", "xml-invalid-character");
    }
    offset += width;
  }

  const documentStart = source.charCodeAt(0) === 0xfeff ? 1 : 0;
  let index = documentStart;
  let rootSeen = false;
  let doctypeSeen = false;
  let doctypeName = "";
  let declarationSeen = false;
  const declaredEntities = new Set<string>();
  const unparsedEntities = new Set<string>();
  const declaredParameterEntities = new Set<string>();

  const skipWhitespace = () => {
    const from = index;
    while (index < source.length && /[\u0020\t\r\n]/u.test(source[index])) index += 1;
    return index > from;
  };
  const readName = () => {
    const name = source.slice(index).match(XML_NAME)?.[0] ?? "";
    index += name.length;
    return name;
  };

  while (index < source.length) {
    if (source.startsWith("<!--", index)) {
      const end = source.indexOf("-->", index + 4);
      if (end < 0) return xmlError(index, source.length, "XML-комментарий не закрыт", "xml-unterminated-comment");
      const inner = source.slice(index + 4, end);
      const doubleDash = inner.indexOf("--");
      if (doubleDash >= 0 || inner.endsWith("-")) {
        const invalidAt = doubleDash >= 0 ? doubleDash : Math.max(0, inner.length - 1);
        return xmlError(index + 4 + invalidAt, index + 5 + invalidAt, "Некорректное содержимое XML-комментария", "xml-invalid-comment");
      }
      index = end + 3;
      continue;
    }
    if (source.startsWith("<![CDATA[", index)) {
      if (!stack.length) return xmlError(index, index + 9, "CDATA допустим только внутри корневого элемента", "xml-cdata-outside-root");
      const end = source.indexOf("]]>", index + 9);
      if (end < 0) return xmlError(index, source.length, "CDATA не закрыт", "xml-unterminated-cdata");
      index = end + 3;
      continue;
    }
    if (source.startsWith("<?", index)) {
      const end = source.indexOf("?>", index + 1);
      if (end < 0) return xmlError(index, source.length, "Processing instruction не закрыта", "xml-unterminated-processing");
      const body = source.slice(index + 2, end);
      const target = body.match(XML_NAME)?.[0] ?? "";
      if (!target) return xmlError(index, end + 2, "У processing instruction нет target", "xml-expected-processing-target");
      const rest = body.slice(target.length);
      if (rest && !/^[ \t\r\n]/u.test(rest)) {
        return xmlError(index + 2 + target.length, end, "Данные processing instruction должны отделяться пробелом", "xml-invalid-processing");
      }
      if (target.toLowerCase() === "xml") {
        const declaration = source.slice(index, end + 2);
        if (
          target !== "xml"
          || declarationSeen
          || index !== documentStart
          || !XML_DECLARATION.test(declaration)
        ) {
          return xmlError(index, end + 2, "Некорректная или неверно расположенная XML-декларация", "xml-invalid-declaration");
        }
        declarationSeen = true;
      }
      index = end + 2;
      continue;
    }
    if (source.startsWith("<!DOCTYPE", index)) {
      if (doctypeSeen || rootSeen || stack.length) {
        return xmlError(index, index + 9, "DOCTYPE должен находиться один раз перед корневым элементом", "xml-invalid-doctype-position");
      }
      let nameAt = index + 9;
      if (!/[ \t\r\n]/u.test(source[nameAt] ?? "")) {
        return xmlError(index, Math.min(source.length, index + 9), "После DOCTYPE требуется имя корневого элемента", "xml-invalid-doctype");
      }
      while (/[ \t\r\n]/u.test(source[nameAt] ?? "")) nameAt += 1;
      doctypeName = source.slice(nameAt).match(XML_NAME)?.[0] ?? "";
      if (!doctypeName) return xmlError(nameAt, Math.min(source.length, nameAt + 1), "После DOCTYPE требуется имя корневого элемента", "xml-invalid-doctype");
      const scanned = scanDoctype(source, nameAt + doctypeName.length);
      if (!scanned.closed) return xmlError(index, source.length, "DOCTYPE не закрыт", "xml-unterminated-doctype");
      const doctypeDiagnostics = validateDoctypeBody(
        source,
        nameAt + doctypeName.length,
        scanned.end - 1,
        declaredEntities,
        unparsedEntities,
        declaredParameterEntities,
      );
      if (doctypeDiagnostics.length) return doctypeDiagnostics;
      doctypeSeen = true;
      index = scanned.end;
      continue;
    }
    if (source[index] !== "<") {
      const end = source.indexOf("<", index);
      const textEnd = end < 0 ? source.length : end;
      if (!stack.length && /[^\u0020\t\r\n]/u.test(source.slice(index, textEnd))) {
        return xmlError(index, textEnd, "Текст вне корневого XML-элемента недопустим", "xml-text-outside-root");
      }
      const cdataClose = source.indexOf("]]>", index);
      if (cdataClose >= 0 && cdataClose < textEnd) {
        return xmlError(cdataClose, cdataClose + 3, "]]> нельзя использовать в обычном XML-тексте", "xml-invalid-cdata-close");
      }
      const entityDiagnostics = checkEntities(source, index, textEnd, declaredEntities, unparsedEntities);
      if (entityDiagnostics.length) return entityDiagnostics;
      index = textEnd;
      continue;
    }
    if (source.startsWith("</", index)) {
      const tagFrom = index;
      index += 2;
      const name = readName();
      if (!name) return xmlError(tagFrom, Math.min(source.length, tagFrom + 2), "У закрывающего тега нет имени", "xml-expected-tag-name");
      skipWhitespace();
      if (source[index] !== ">") {
        return xmlError(index, Math.min(source.length, index + 1), "В закрывающем теге после имени допустим только >", "xml-invalid-closing-tag");
      }
      index += 1;
      const expected = stack.at(-1);
      if (expected !== name) {
        return xmlError(tagFrom, index, expected
          ? `Закрывающий тег </${name}> не совпадает с <${expected}>`
          : `Лишний закрывающий тег </${name}>`, "xml-mismatched-tag");
      }
      stack.pop();
      continue;
    }
    if (source.startsWith("<!", index)) {
      return xmlError(index, Math.min(source.length, index + 2), "Неизвестная XML-декларация", "xml-invalid-declaration");
    }

    const tagFrom = index++;
    const name = readName();
    if (!name) return xmlError(tagFrom, Math.min(source.length, tagFrom + 1), "У открывающего тега нет имени", "xml-expected-tag-name");
    if (!stack.length) {
      if (rootSeen) return xmlError(tagFrom, index, "XML-документ может иметь только один корневой элемент", "xml-multiple-roots");
      if (doctypeName && doctypeName !== name) {
        return xmlError(tagFrom + 1, index, `Корневой элемент <${name}> не совпадает с DOCTYPE ${doctypeName}`, "xml-doctype-root-mismatch");
      }
      rootSeen = true;
    }

    const attributes = new Set<string>();
    let selfClosing = false;
    while (index < source.length) {
      const separated = skipWhitespace();
      if (source.startsWith("/>", index)) {
        index += 2;
        selfClosing = true;
        break;
      }
      if (source[index] === ">") {
        index += 1;
        break;
      }
      if (!separated) {
        return xmlError(index, Math.min(source.length, index + 1), "XML-атрибут должен отделяться пробелом", "xml-expected-whitespace");
      }
      const attributeFrom = index;
      const attribute = readName();
      if (!attribute) return xmlError(attributeFrom, Math.min(source.length, attributeFrom + 1), "Ожидалось имя XML-атрибута", "xml-expected-attribute");
      if (attributes.has(attribute)) return xmlError(attributeFrom, index, `Атрибут ${attribute} указан повторно`, "xml-duplicate-attribute");
      attributes.add(attribute);
      skipWhitespace();
      if (source[index] !== "=") return xmlError(index, index, `После атрибута ${attribute} требуется =`, "xml-expected-equals");
      index += 1;
      skipWhitespace();
      const quote = source[index];
      if (quote !== '"' && quote !== "'") {
        return xmlError(index, Math.min(source.length, index + 1), "Значение XML-атрибута должно быть в кавычках", "xml-unquoted-attribute");
      }
      const valueFrom = ++index;
      const valueEnd = source.indexOf(quote, valueFrom);
      if (valueEnd < 0) return xmlError(valueFrom - 1, source.length, "Значение XML-атрибута не закрыто", "xml-unterminated-attribute");
      const lessThan = source.indexOf("<", valueFrom);
      if (lessThan >= 0 && lessThan < valueEnd) return xmlError(lessThan, lessThan + 1, "Символ < недопустим в XML-атрибуте", "xml-invalid-attribute-value");
      const entityDiagnostics = checkEntities(source, valueFrom, valueEnd, declaredEntities, unparsedEntities);
      if (entityDiagnostics.length) return entityDiagnostics;
      index = valueEnd + 1;
    }
    if (index > source.length || (!selfClosing && source[index - 1] !== ">")) {
      return xmlError(tagFrom, source.length, `Тег <${name}> не закрыт`, "xml-unterminated-tag");
    }
    if (!selfClosing) {
      if (stack.length >= MAX_VALIDATION_DEPTH) {
        return xmlError(tagFrom, index, "Превышена допустимая глубина XML", "xml-depth-limit");
      }
      stack.push(name);
    }
  }

  if (stack.length) return xmlError(source.length, source.length, `Тег <${stack.at(-1)}> не закрыт`, "xml-unclosed-tag");
  if (!rootSeen) return xmlError(index, index, "В XML-документе нет корневого элемента", "xml-missing-root");
  return [];
}

export const xmlLanguage: LanguageDefinition<XmlLexerState> = {
  id: "xml",
  name: "XML",
  aliases: ["xsd", "xsl", "xslt", "svg", "plist"],
  extensions: ["xml", "xsd", "xsl", "xslt", "svg", "plist", "csproj", "props", "targets"],
  mimeTypes: ["application/xml", "text/xml", "image/svg+xml"],
  validationLevel: "structural",
  initialState: state,
  statesEqual: xmlStatesEqual,
  stateKey: (value) => [
    value.mode,
    value.quote,
    value.tagName,
    value.closingTag ? 1 : 0,
    value.overflowDepth,
    value.doctypeDepth,
    value.doctypeQuote,
    value.doctypeComment ? 1 : 0,
    value.doctypeProcessing ? 1 : 0,
    value.stack?.depth ?? 0,
    value.stack?.hash ?? 0,
  ].join("|"),
  validate: validateXml,
  finalize(source, endState) {
    if (endState.mode !== "text") {
      return xmlError(source.length, source.length, "XML-конструкция не закрыта", `xml-unterminated-${endState.mode}`);
    }
    if (endState.overflowDepth > 0 || endState.stack) {
      return xmlError(source.length, source.length, `Тег <${endState.stack?.name ?? "…"}> не закрыт`, "xml-unclosed-tag");
    }
    return [];
  },
  tokenizeLine(line, startState) {
    const tokens: LineToken[] = [];
    const diagnostics: LineDiagnostic[] = [];
    let current = startState as XmlLexerState;
    let index = 0;
    const push = (from: number, to: number, kind: TokenKind) => {
      if (to > from) tokens.push({ from, to, kind });
    };

    if (current.mode === "comment") {
      const continuation = continueUntil(line, "-->", "comment", current, "text");
      tokens.push(...continuation.tokens);
      if (continuation.index === undefined) return { tokens, diagnostics, state: current };
      index = continuation.index;
      current = continuation.state;
    } else if (current.mode === "cdata") {
      const continuation = continueUntil(line, "]]>", "string", current, "text");
      tokens.push(...continuation.tokens);
      if (continuation.index === undefined) return { tokens, diagnostics, state: current };
      index = continuation.index;
      current = continuation.state;
    } else if (current.mode === "processing") {
      const continuation = continueUntil(line, "?>", "directive", current, "text");
      tokens.push(...continuation.tokens);
      if (continuation.index === undefined) return { tokens, diagnostics, state: current };
      index = continuation.index;
      current = continuation.state;
    } else if (current.mode === "doctype") {
      const scanned = scanDoctype(
        line,
        0,
        current.doctypeDepth,
        current.doctypeQuote,
        current.doctypeComment,
        current.doctypeProcessing,
      );
      push(0, scanned.end, "directive");
      if (!scanned.closed) {
        return {
          tokens,
          diagnostics,
          state: state({
            ...current,
            doctypeDepth: scanned.depth,
            doctypeQuote: scanned.quote,
            doctypeComment: scanned.comment,
            doctypeProcessing: scanned.processing,
          }),
        };
      }
      index = scanned.end;
      current = state({
        ...current,
        mode: "text",
        doctypeDepth: 0,
        doctypeQuote: "",
        doctypeComment: false,
        doctypeProcessing: false,
      });
    } else if (current.mode === "attribute") {
      const end = line.indexOf(current.quote);
      if (end < 0) {
        push(0, line.length, "string");
        return { tokens, diagnostics, state: current };
      }
      push(0, end + 1, "string");
      index = end + 1;
      current = state({ ...current, mode: "tag", quote: "" });
    }

    while (index < line.length) {
      if (current.mode === "text") {
        if (line.startsWith("<!--", index)) {
          const end = line.indexOf("-->", index + 4);
          if (end < 0) {
            push(index, line.length, "comment");
            current = state({ ...current, mode: "comment" });
            break;
          }
          push(index, end + 3, "comment");
          index = end + 3;
          continue;
        }
        if (line.startsWith("<![CDATA[", index)) {
          const end = line.indexOf("]]>", index + 9);
          if (end < 0) {
            push(index, line.length, "string");
            current = state({ ...current, mode: "cdata" });
            break;
          }
          push(index, end + 3, "string");
          index = end + 3;
          continue;
        }
        if (line.startsWith("<?", index)) {
          const end = line.indexOf("?>", index + 2);
          if (end < 0) {
            push(index, line.length, "directive");
            current = state({ ...current, mode: "processing" });
            break;
          }
          push(index, end + 2, "directive");
          index = end + 2;
          continue;
        }
        if (/^<!DOCTYPE\b/u.test(line.slice(index))) {
          const scanned = scanDoctype(line, index + 9);
          push(index, scanned.end, "directive");
          if (!scanned.closed) {
            current = state({
              ...current,
              mode: "doctype",
              doctypeDepth: scanned.depth,
              doctypeQuote: scanned.quote,
              doctypeComment: scanned.comment,
              doctypeProcessing: scanned.processing,
            });
            break;
          }
          index = scanned.end;
          continue;
        }
        if (line[index] === "<") {
          const closingTag = line[index + 1] === "/";
          push(index, index + (closingTag ? 2 : 1), "punctuation");
          index += closingTag ? 2 : 1;
          current = state({ ...current, mode: "tag", tagName: "", closingTag });
          continue;
        }
        const entity = line.slice(index).match(ENTITY)?.[0];
        if (entity) {
          push(index, index + entity.length, "escape");
          index += entity.length;
          continue;
        }
        const from = index++;
        while (index < line.length && line[index] !== "<" && line[index] !== "&") index += 1;
        push(from, index, /^\s*$/u.test(line.slice(from, index)) ? "whitespace" : "text");
        continue;
      }

      if (/\s/u.test(line[index])) {
        const from = index++;
        while (index < line.length && /\s/u.test(line[index])) index += 1;
        push(from, index, "whitespace");
        continue;
      }
      if (line.startsWith("/>", index)) {
        push(index, index + 2, "punctuation");
        index += 2;
        current = state({ ...current, mode: "text", tagName: "", closingTag: false });
        continue;
      }
      if (line[index] === ">") {
        push(index, index + 1, "punctuation");
        if (current.closingTag) {
          if (current.overflowDepth > 0) {
            current = state({ ...current, overflowDepth: current.overflowDepth - 1, mode: "text", tagName: "", closingTag: false });
          } else {
            const expected = current.stack?.name;
            if (expected !== current.tagName) {
              diagnostics.push({
                from: Math.max(0, index - current.tagName.length - 2),
                to: index + 1,
                severity: "error",
                code: "xml-mismatched-tag",
                message: expected
                  ? `Закрывающий тег </${current.tagName}> не совпадает с <${expected}>`
                  : `Лишний закрывающий тег </${current.tagName}>`,
              });
            }
            let match = current.stack;
            while (match && match.name !== current.tagName) match = match.parent;
            current = state({
              ...current,
              stack: match ? match.parent : current.stack,
              mode: "text",
              tagName: "",
              closingTag: false,
            });
          }
        } else if (current.overflowDepth > 0) {
          current = state({
            ...current,
            overflowDepth: current.overflowDepth + 1,
            mode: "text",
            tagName: "",
          });
        } else if ((current.stack?.depth ?? 0) >= MAX_LEXICAL_DEPTH) {
          diagnostics.push({
            from: Math.max(0, index - current.tagName.length - 1),
            to: index + 1,
            severity: "error",
            code: "xml-depth-limit",
            message: "Превышена допустимая глубина XML",
          });
          current = state({ ...current, overflowDepth: current.overflowDepth + 1, mode: "text", tagName: "" });
        } else {
          current = state({
            ...current,
            stack: current.tagName ? pushTag(current.stack, current.tagName) : current.stack,
            mode: "text",
            tagName: "",
          });
        }
        index += 1;
        continue;
      }
      if (line[index] === "=") {
        push(index, index + 1, "operator");
        index += 1;
        continue;
      }
      if (line[index] === '"' || line[index] === "'") {
        const quote = line[index] as '"' | "'";
        const end = line.indexOf(quote, index + 1);
        if (end < 0) {
          push(index, line.length, "string");
          current = state({ ...current, mode: "attribute", quote });
          break;
        }
        push(index, end + 1, "string");
        index = end + 1;
        continue;
      }
      const name = line.slice(index).match(XML_NAME)?.[0] ?? "";
      if (name) {
        const tagName = current.tagName || name;
        push(index, index + name.length, current.tagName ? "attribute" : "tag");
        current = state({ ...current, tagName });
        index += name.length;
        continue;
      }
      push(index, index + 1, "invalid");
      diagnostics.push({
        from: index,
        to: index + 1,
        severity: "error",
        code: "xml-invalid-token",
        message: "Недопустимый символ внутри XML-тега",
      });
      index += 1;
    }

    return { tokens, diagnostics, state: current };
  },
};
