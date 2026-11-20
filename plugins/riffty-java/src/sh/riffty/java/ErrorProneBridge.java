package sh.riffty.java;

import com.google.errorprone.BaseErrorProneJavaCompiler;
import com.google.errorprone.DescriptionListener;
import com.google.errorprone.ErrorProneOptions;
import com.google.errorprone.bugpatterns.BugChecker;
import com.google.errorprone.fixes.Fix;
import com.google.errorprone.fixes.Replacement;
import com.google.errorprone.matchers.Description;
import com.google.errorprone.scanner.BuiltInCheckerSuppliers;
import com.google.errorprone.scanner.ErrorProneScannerTransformer;
import com.google.errorprone.scanner.ScannerSupplier;
import com.sun.source.tree.CompilationUnitTree;
import com.sun.source.util.TreePath;
import com.sun.tools.javac.tree.EndPosTable;
import com.sun.tools.javac.tree.JCTree;
import com.sun.tools.javac.util.Context;
import java.net.URI;
import java.util.*;

/**
 * Programmatic Google Error Prone + Picnic error-prone-contrib bridge (RFC §4.4).
 * Runs directly on the already-analyzed JavacTask CompilationUnitTrees + Context in memory
 * (zero duplicate javac pass, zero `-proc` execution, never uses `-XepPatchLocation:IN_PLACE`),
 * capturing `Description` + `SuggestedFix` replacements and mapping UTF-16 offsets to UTF-8 bytes.
 */
public final class ErrorProneBridge {

    private static final Set<String> SAFE_AUTOFIX_CHECKERS = Set.of(
        "BigDecimalLiteralDouble",
        "BoxedPrimitiveConstructor",
        "StringBuilderInitWithChar",
        "UnnecessaryBoxedVariable",
        "RedundantOverride",
        "RedundantStringConversion",
        "PrimitiveComparison"
    );

    private static final Set<String> PICNIC_CHECKERS = Set.of(
        "OptionalOrElseGet",
        "EagerStringFormatting",
        "RedundantStringConversion",
        "PrimitiveComparison",
        "FormatStringConcatenation",
        "NestedOptionals",
        "FluxImplicitBlock"
    );

    private static final Set<String> ENABLED_CHECKERS = Set.of(
        // Google Error Prone core high-signal bug & FinOps checkers
        "BigDecimalLiteralDouble",
        "StreamResourceLeak",
        "EqualsHashCode",
        "FutureReturnValueIgnored",
        "BoxedPrimitiveConstructor",
        "ReturnValueIgnored",
        "ModifiedButNotUsed",
        "StringBuilderInitWithChar",
        "SelfEquals",
        "SelfComparison",
        "CollectionIncompatibleType",
        "IdentityBinaryExpression",
        // Picnic error-prone-contrib high-signal checkers
        "OptionalOrElseGet",
        "EagerStringFormatting",
        "RedundantStringConversion",
        "PrimitiveComparison",
        "FormatStringConcatenation",
        "NestedOptionals",
        "FluxImplicitBlock"
    );

    private static volatile ErrorProneScannerTransformer cachedTransformer;

    private ErrorProneBridge() {}

    private static ErrorProneScannerTransformer getTransformer() {
        ErrorProneScannerTransformer t = cachedTransformer;
        if (t != null) return t;
        synchronized (ErrorProneBridge.class) {
            if (cachedTransformer != null) return cachedTransformer;
            ScannerSupplier base = BuiltInCheckerSuppliers.allChecks()
                .filter(info -> ENABLED_CHECKERS.contains(info.canonicalName()));

            List<Class<? extends BugChecker>> picnicClasses = new ArrayList<>();
            try {
                for (BugChecker checker : ServiceLoader.load(BugChecker.class, ErrorProneBridge.class.getClassLoader())) {
                    String name = checker.canonicalName();
                    if (PICNIC_CHECKERS.contains(name)) {
                        picnicClasses.add(checker.getClass());
                    }
                }
            } catch (Throwable _) {
                // ServiceLoader failure should never block built-in Error Prone checks
            }

            ScannerSupplier combined = picnicClasses.isEmpty()
                ? base
                : base.plus(ScannerSupplier.fromBugCheckerClasses(picnicClasses));
            cachedTransformer = ErrorProneScannerTransformer.create(combined.get());
            return cachedTransformer;
        }
    }

    public static List<Map<String, Object>> analyze(
        Context context,
        Iterable<? extends CompilationUnitTree> parsedUnits,
        Map<URI, JavaAnalyzerEngine.FileInput> uriToFile,
        Map<URI, Utf16ToUtf8Index> uriToUtf8
    ) {
        List<Map<String, Object>> out = new ArrayList<>();
        if (context == null) return out;

        try {
            if (context.get(ErrorProneOptions.class) == null) {
                context.put(ErrorProneOptions.class, ErrorProneOptions.empty());
            }
            BaseErrorProneJavaCompiler.setupMessageBundle(context);
            ErrorProneScannerTransformer transformer = getTransformer();

            for (CompilationUnitTree cu : parsedUnits) {
                if (!(cu instanceof JCTree.JCCompilationUnit jcUnit)) continue;
                URI uri = cu.getSourceFile().toUri();
                JavaAnalyzerEngine.FileInput fi = uriToFile.get(uri);
                Utf16ToUtf8Index utf8 = uriToUtf8.get(uri);
                if (fi == null || utf8 == null) continue;

                EndPosTable endPos = jcUnit.endPositions;
                DescriptionListener listener = description -> {
                    try {
                        int startChar = description.position != null
                            ? Math.max(0, description.position.getStartPosition())
                            : 0;
                        int endChar = (description.position != null && endPos != null)
                            ? Math.max(startChar, description.position.getEndPosition(endPos))
                            : startChar;

                        String checkName = description.checkName;
                        boolean isPicnic = PICNIC_CHECKERS.contains(checkName);
                        String ruleId = (isPicnic ? "PICNIC-" : "EP-") + checkName;
                        String severity = switch (description.severity()) {
                            case ERROR -> "Error";
                            case WARNING -> "Warning";
                            default -> "Info";
                        };

                        List<Map<String, Object>> edits = new ArrayList<>();
                        String applicability = "ManualOnly";
                        if (SAFE_AUTOFIX_CHECKERS.contains(checkName)
                                && !description.fixes.isEmpty()
                                && endPos != null) {
                            Fix firstFix = description.fixes.getFirst();
                            Set<Replacement> replacements = firstFix.getReplacements(endPos);
                            for (Replacement rep : replacements) {
                                int rs = utf8.toByteOffset(rep.startPosition());
                                int re = utf8.toByteOffset(rep.endPosition());
                                Map<String, Object> rangeMap = new LinkedHashMap<>();
                                rangeMap.put("start", rs);
                                rangeMap.put("end", re);
                                Map<String, Object> editMap = new LinkedHashMap<>();
                                editMap.put("range", rangeMap);
                                editMap.put("replacement", rep.replaceWith());
                                edits.add(editMap);
                            }
                            if (!edits.isEmpty()) {
                                applicability = "Safe";
                            }
                        }

                        Map<String, Object> rangeMap = new LinkedHashMap<>();
                        rangeMap.put("start", utf8.toByteOffset(startChar));
                        rangeMap.put("end", utf8.toByteOffset(endChar));

                        Map<String, Object> fixMap = null;
                        if (!edits.isEmpty()) {
                            fixMap = new LinkedHashMap<>();
                            fixMap.put("title", (isPicnic ? "Picnic " : "Error Prone ") + checkName + " remediation");
                            fixMap.put("edits", edits);
                            fixMap.put("required_imports", List.of());
                            fixMap.put("applicability", applicability);
                        }

                        Map<String, Object> proof = new LinkedHashMap<>();
                        proof.put("tier", "tier2_semantic");
                        proof.put("engine", isPicnic ? "picnic-error-prone-contrib" : "google-error-prone");
                        proof.put("checker", checkName);

                        String rawMsg = description.getRawMessage();
                        int nl = rawMsg != null ? rawMsg.indexOf('\n') : -1;
                        String cleanMsg = rawMsg == null ? checkName : (nl > 0 ? rawMsg.substring(0, nl).trim() : rawMsg.trim());

                        Map<String, Object> diag = new LinkedHashMap<>();
                        diag.put("rule_id", ruleId);
                        diag.put("rule_name", checkName);
                        diag.put("severity", severity);
                        diag.put("file_id", fi.fileId());
                        diag.put("range", rangeMap);
                        diag.put("message", cleanMsg);
                        diag.put("type_proof", proof);
                        diag.put("fix", fixMap);
                        out.add(diag);
                    } catch (Throwable _) {
                        // Never fail the compilation unit on a single Error Prone description translation
                    }
                };

                try {
                    transformer.apply(new TreePath(cu), context, listener);
                } catch (Throwable _) {
                    // Partial classpath attribution errors on individual units are tolerated
                }
            }
        } catch (Throwable _) {
            // Gracefully tolerate Error Prone initialization errors
        }
        return out;
    }
}
