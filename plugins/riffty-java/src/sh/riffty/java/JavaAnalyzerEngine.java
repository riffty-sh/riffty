package sh.riffty.java;

import com.sun.source.tree.*;
import com.sun.source.util.JavacTask;
import com.sun.source.util.SourcePositions;
import com.sun.source.util.TreePath;
import com.sun.source.util.TreePathScanner;
import com.sun.source.util.TreeScanner;
import com.sun.source.util.Trees;
import java.io.File;
import java.io.IOException;
import java.lang.classfile.ClassFile;
import java.lang.classfile.ClassModel;
import java.net.URI;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.*;
import javax.lang.model.type.TypeKind;
import javax.lang.model.type.TypeMirror;
import javax.tools.DiagnosticCollector;
import javax.tools.JavaCompiler;
import javax.tools.JavaFileObject;
import javax.tools.SimpleJavaFileObject;
import javax.tools.StandardJavaFileManager;
import javax.tools.ToolProvider;

/**
 * Programmatic JDK 25 LTS javac + Trees + ClassFile (JEP 484) analyzer engine.
 * Strictly runs with `-proc:none` (zero RCE) and allocates a fresh
 * StandardJavaFileManager per call.
 */
public final class JavaAnalyzerEngine {

    private static final Set<String> OBJECT_MAPPER_MUTATORS = Set.of(
            "configure", "enable", "disable", "registerModule", "registerModules",
            "findAndRegisterModules", "addMixIn", "activateDefaultTyping",
            "deactivateDefaultTyping", "setDateFormat", "setSerializationInclusion",
            "setPropertyNamingStrategy", "setVisibility", "setTimeZone", "setLocale",
            "setDefaultPropertyInclusion", "setFilterProvider", "setSubtypeResolver",
            "setAnnotationIntrospector", "setNodeFactory", "setMixIns");

    private static final Set<String> PATTERN_FLAG_CONSTANTS = Set.of(
            "UNIX_LINES", "CASE_INSENSITIVE", "COMMENTS", "MULTILINE",
            "LITERAL", "DOTALL", "UNICODE_CASE", "CANON_EQ", "UNICODE_CHARACTER_CLASS");

    public record ArchForbidRule(String id, String from, List<String> targets, String message) {
    }

    public record FileInput(int fileId, String relativePath, String source) {
    }

    public record AnalyzeResult(
            List<Map<String, Object>> diagnostics,
            Map<String, Object> provenance) {
    }

    public AnalyzeResult analyze(
            Path workspaceRoot,
            List<String> classpath,
            List<FileInput> files,
            List<ArchForbidRule> archRules,
            String modeTag) throws IOException {
        JavaCompiler compiler = ToolProvider.getSystemJavaCompiler();
        if (compiler == null) {
            throw new IllegalStateException("System JavaCompiler unavailable — JDK required");
        }

        List<Map<String, Object>> allDiags = new ArrayList<>();
        int classfileMajor = detectCompiledClassfileMajor(workspaceRoot);

        // Fresh StandardJavaFileManager per request (thread-safe for Virtual Threads in
        // daemon mode)
        try (StandardJavaFileManager stdFm = compiler.getStandardFileManager(null, Locale.ROOT,
                StandardCharsets.UTF_8)) {

            List<JavaFileObject> units = new ArrayList<>(files.size());
            Map<URI, FileInput> uriToFile = new HashMap<>();
            Map<URI, Utf16ToUtf8Index> uriToUtf8 = new HashMap<>();
            for (FileInput fi : files) {
                InMemoryJavaFile obj = new InMemoryJavaFile(fi.relativePath(), fi.source());
                units.add(obj);
                uriToFile.put(obj.toUri(), fi);
                uriToUtf8.put(obj.toUri(), new Utf16ToUtf8Index(fi.source()));
            }

            List<String> options = new ArrayList<>();
            options.add("-proc:none"); // CRITICAL: zero RCE, never run repo annotation processors
            options.add("-implicit:none");
            options.add("-Xlint:none");
            if (!classpath.isEmpty()) {
                options.add("-classpath");
                options.add(String.join(File.pathSeparator, classpath));
            }

            DiagnosticCollector<JavaFileObject> diagCollector = new DiagnosticCollector<>();
            JavacTask task = (JavacTask) compiler.getTask(
                    null, stdFm, diagCollector, options, null, units);

            Iterable<? extends CompilationUnitTree> parsedUnits = task.parse();
            try {
                task.analyze();
            } catch (Throwable _) {
                // Partial classpath attribution errors are tolerated; Trees still resolves JDK
                // & local types.
            }

            Trees trees = Trees.instance(task);
            SourcePositions positions = trees.getSourcePositions();

            for (CompilationUnitTree cu : parsedUnits) {
                FileInput fi = uriToFile.get(cu.getSourceFile().toUri());
                if (fi == null)
                    continue;
                Utf16ToUtf8Index utf8 = uriToUtf8.get(cu.getSourceFile().toUri());
                UnitAnalyzer ua = new UnitAnalyzer(fi, cu, trees, positions, utf8, archRules);
                ua.run();
                allDiags.addAll(ua.diagnostics);
            }

            // Run Google Error Prone + Picnic error-prone-contrib pass on the already-analyzed units
            com.sun.tools.javac.util.Context javacContext =
                (task instanceof com.sun.tools.javac.api.BasicJavacTask bjt) ? bjt.getContext() : null;
            List<Map<String, Object>> epDiags =
                ErrorProneBridge.analyze(javacContext, parsedUnits, uriToFile, uriToUtf8);
            Set<String> seenSpans = new HashSet<>();
            for (Map<String, Object> d : allDiags) {
                @SuppressWarnings("unchecked")
                Map<String, Object> r = (Map<String, Object>) d.get("range");
                seenSpans.add(d.get("file_id") + ":" + r.get("start"));
            }
            for (Map<String, Object> epd : epDiags) {
                @SuppressWarnings("unchecked")
                Map<String, Object> r = (Map<String, Object>) epd.get("range");
                String key = epd.get("file_id") + ":" + r.get("start");
                if (seenSpans.add(key)) {
                    allDiags.add(epd);
                }
            }
        }

        StringBuilder detail = new StringBuilder();
        detail.append("jdk ").append(Runtime.version().feature())
                .append(" (javac + error-prone + picnic · ").append(modeTag).append(")");
        if (!classpath.isEmpty()) {
            detail.append(" · ").append(classpath.size()).append(" cp entries");
        }
        if (classfileMajor > 0) {
            detail.append(" · bytecode v").append(classfileMajor - 44);
        }

        Map<String, Object> provenance = new LinkedHashMap<>();
        provenance.put("language", "java");
        provenance.put("tier", "tier2_semantic");
        provenance.put("files_analyzed", files.size());
        provenance.put("runtime_detail", detail.toString());
        provenance.put("degraded_reason", null);
        provenance.put("remediation_hint", null);

        return new AnalyzeResult(allDiags, provenance);
    }

    /**
     * Uses Java 25's standard Class-File API (JEP 484) to inspect compiled bytecode
     * without ASM.
     */
    private static int detectCompiledClassfileMajor(Path workspaceRoot) {
        if (workspaceRoot == null)
            return 0;
        for (String rel : List.of("target/classes", "build/classes/java/main")) {
            Path dir = workspaceRoot.resolve(rel);
            if (!Files.isDirectory(dir))
                continue;
            try (var stream = Files.walk(dir, 6)) {
                Optional<Path> firstClass = stream
                        .filter(p -> p.toString().endsWith(".class"))
                        .findFirst();
                if (firstClass.isPresent()) {
                    ClassModel model = ClassFile.of().parse(firstClass.get());
                    return model.majorVersion();
                }
            } catch (IOException _) {
                // ignore unreadable bytecode dirs
            }
        }
        return 0;
    }

    private static final class InMemoryJavaFile extends SimpleJavaFileObject {
        private final String source;

        InMemoryJavaFile(String relativePath, String source) {
            super(
                    URI.create("string:///" + relativePath.replace('\\', '/').replace(" ", "%20")),
                    Kind.SOURCE);
            this.source = source;
        }

        @Override
        public CharSequence getCharContent(boolean ignoreEncodingErrors) {
            return source;
        }
    }

    private static final class UnitAnalyzer extends TreePathScanner<Void, Void> {
        final FileInput fi;
        final CompilationUnitTree cu;
        final Trees trees;
        final SourcePositions positions;
        final Utf16ToUtf8Index utf8;
        final List<ArchForbidRule> archRules;
        final List<Map<String, Object>> diagnostics = new ArrayList<>();
        final Set<ClassTree> hoistedFieldScheduled = Collections.newSetFromMap(new IdentityHashMap<>());
        final Set<ClassTree> hoistedHttpFieldScheduled = Collections.newSetFromMap(new IdentityHashMap<>());
        final Map<ClassTree, Map<String, String>> hoistedPatternsByClass = new IdentityHashMap<>();
        final Map<ClassTree, Set<String>> usedFieldNamesByClass = new IdentityHashMap<>();

        UnitAnalyzer(
                FileInput fi,
                CompilationUnitTree cu,
                Trees trees,
                SourcePositions positions,
                Utf16ToUtf8Index utf8,
                List<ArchForbidRule> archRules) {
            this.fi = fi;
            this.cu = cu;
            this.trees = trees;
            this.positions = positions;
            this.utf8 = utf8;
            this.archRules = archRules;
        }

        void run() {
            checkArchitectureImports();
            scan(new TreePath(cu), null);
        }

        private void checkArchitectureImports() {
            if (archRules.isEmpty())
                return;
            String pkgName = cu.getPackageName() != null ? cu.getPackageName().toString() : "";
            for (ArchForbidRule rule : archRules) {
                if (!matchesPackageGlob(pkgName, rule.from()))
                    continue;
                for (ImportTree imp : cu.getImports()) {
                    String imported = imp.getQualifiedIdentifier().toString();
                    for (String forbidden : rule.targets()) {
                        if (matchesPackageGlob(imported, forbidden)) {
                            int start = (int) positions.getStartPosition(cu, imp);
                            int end = (int) positions.getEndPosition(cu, imp);
                            if (start < 0 || end < start)
                                continue;
                            if (isLineSuppressed(start, rule.id()))
                                continue;
                            Map<String, Object> proof = new LinkedHashMap<>();
                            proof.put("package", pkgName);
                            proof.put("forbidden_import", imported);
                            proof.put("rule_pattern", forbidden);
                            diagnostics.add(makeDiag(
                                    rule.id(),
                                    "architecture-forbid",
                                    "Error",
                                    utf8.toByteOffset(start),
                                    utf8.toByteOffset(end),
                                    rule.message() + " (forbidden import `" + imported + "` in `" + pkgName + "`)",
                                    proof,
                                    null));
                        }
                    }
                }
            }
        }

        @Override
        public Void visitNewClass(NewClassTree node, Void unused) {
            String ident = node.getIdentifier().toString();
            String simple = ident.contains(".") ? ident.substring(ident.lastIndexOf('.') + 1) : ident;
            int start = (int) positions.getStartPosition(cu, node);
            int end = (int) positions.getEndPosition(cu, node);

            if (start >= 0 && end >= start) {
                switch (simple) {
                    case "BigDecimal" -> checkBigDecimal(node, start, end);
                    case "Integer", "Long", "Double", "Boolean" -> checkPrimitiveBox(node, simple, start, end);
                    case "ObjectMapper" -> checkObjectMapper(node, start, end);
                    case "OkHttpClient" -> checkUnpooledHttp(node, "new OkHttpClient()", start, end);
                    default -> {
                    }
                }
            }
            return super.visitNewClass(node, unused);
        }

        @Override
        public Void visitMethodInvocation(MethodInvocationTree node, Void unused) {
            String expr = node.toString();
            int start = (int) positions.getStartPosition(cu, node);
            int end = (int) positions.getEndPosition(cu, node);
            if (start >= 0 && end >= start) {
                if (expr.equals("HttpClient.newHttpClient()")) {
                    checkUnpooledHttp(node, "HttpClient.newHttpClient()", start, end);
                } else if (expr.endsWith(".collect(Collectors.toList()).isEmpty()")
                        && node.getMethodSelect() instanceof MemberSelectTree ms
                        && ms.getExpression() instanceof MethodInvocationTree collectCall
                        && collectCall.getMethodSelect() instanceof MemberSelectTree collectMs) {
                    if (!isLineSuppressed(start, "JAVA-STREAM-001")) {
                        int streamStart = (int) positions.getStartPosition(cu, collectMs.getExpression());
                        int streamEnd = (int) positions.getEndPosition(cu, collectMs.getExpression());
                        if (streamStart >= 0 && streamEnd > streamStart) {
                            String streamSrc = fi.source().substring(streamStart, streamEnd);
                            Map<String, Object> fix = makeFix(
                                    "Replace with short-circuiting `.findAny().isEmpty()`",
                                    List.of(makeEdit(utf8.toByteOffset(start), utf8.toByteOffset(end),
                                            streamSrc + ".findAny().isEmpty()")),
                                    "Safe");
                            diagnostics.add(makeDiag(
                                    "JAVA-STREAM-001", "stream-collect-is-empty", "Warning",
                                    utf8.toByteOffset(start), utf8.toByteOffset(end),
                                    "`collect(Collectors.toList()).isEmpty()` allocates a full temporary List; use `.findAny().isEmpty()`.",
                                    Map.of("tier", "tier2_semantic"),
                                    fix));
                        }
                    }
                } else if (node.getMethodSelect() instanceof MemberSelectTree ms
                        && ms.getExpression().toString().equals("Files")
                        && Set.of("walk", "lines", "list").contains(ms.getIdentifier().toString())) {
                    checkUnclosedFilesStream(ms.getIdentifier().toString(), start, end);
                } else if (node.getMethodSelect() instanceof MemberSelectTree ms
                        && (ms.getExpression().toString().equals("Pattern")
                                || ms.getExpression().toString().equals("java.util.regex.Pattern"))
                        && ms.getIdentifier().toString().equals("compile")) {
                    checkRegexCompile(node, ms, start, end);
                }
            }
            return super.visitMethodInvocation(node, unused);
        }

        @Override
        public Void visitCompoundAssignment(CompoundAssignmentTree node, Void unused) {
            if (node.getKind() == Tree.Kind.PLUS_ASSIGNMENT && isInsideLoop(getCurrentPath())) {
                ExpressionTree rhs = node.getExpression();
                boolean rhsIsString = rhs.getKind() == Tree.Kind.STRING_LITERAL
                        || (rhs.getKind() == Tree.Kind.PLUS && rhs.toString().contains("\""));
                if (rhsIsString) {
                    int start = (int) positions.getStartPosition(cu, node);
                    int end = (int) positions.getEndPosition(cu, node);
                    if (start >= 0 && end >= start && !isLineSuppressed(start, "JAVA-STR-001")) {
                        diagnostics.add(makeDiag(
                                "JAVA-STR-001", "string-concat-in-loop", "Warning",
                                utf8.toByteOffset(start), utf8.toByteOffset(end),
                                "String concatenation with `+=` inside a loop causes O(N^2) allocations; accumulate with `StringBuilder`.",
                                Map.of("tier", "tier2_semantic", "inside_loop", true),
                                makeFix("Replace loop `+=` string concatenation with `StringBuilder`", List.of(),
                                        "ManualOnly")));
                    }
                }
            }
            return super.visitCompoundAssignment(node, unused);
        }

        private void checkRegexCompile(MethodInvocationTree node, MemberSelectTree ms, int start, int end) {
            List<? extends ExpressionTree> args = node.getArguments();
            if (args.size() != 1 && args.size() != 2)
                return;

            TreePath path = getCurrentPath();
            boolean inLoop = isInsideLoop(path);
            MethodTree enclosingMethod = findEnclosingHotMethod(path);
            if (!inLoop && enclosingMethod == null)
                return;

            Tree parent = path.getParentPath() != null ? path.getParentPath().getLeaf() : null;
            if (parent instanceof AssignmentTree && !inLoop) {
                return; // lazy singleton field initialization
            }

            if (isLineSuppressed(start, "JAVA-REGEX-001"))
                return;

            boolean constantRegex = isConstantStringLiteral(args.get(0));
            boolean constantFlags = args.size() == 1 || isConstantPatternFlags(args.get(1));
            boolean isConstant = constantRegex && constantFlags;

            ClassTree enclosingClass = findEnclosingClass(path);
            boolean canHoistToClass = enclosingClass != null
                    && (enclosingClass.getKind() == Tree.Kind.CLASS || enclosingClass.getKind() == Tree.Kind.RECORD)
                    && !enclosingClass.getSimpleName().isEmpty();

            Map<String, Object> proof = new LinkedHashMap<>();
            proof.put("tier", "tier2_semantic");
            proof.put("inside_loop", inLoop);
            proof.put("constant_pattern", isConstant);

            Map<String, Object> fix;
            if (isConstant && canHoistToClass) {
                int insertPos = findClassBodyInsertOffset(enclosingClass);
                if (insertPos >= 0) {
                    String callSrc = fi.source().substring(start, end);
                    String normalizedCall = callSrc.trim();
                    Map<String, String> classHoisted = hoistedPatternsByClass.computeIfAbsent(
                            enclosingClass, _ -> new LinkedHashMap<>());
                    String existingField = classHoisted.get(normalizedCall);
                    List<Map<String, Object>> edits = new ArrayList<>();
                    String fieldName;
                    if (existingField != null) {
                        fieldName = existingField;
                    } else {
                        fieldName = allocatePatternFieldName(enclosingClass);
                        classHoisted.put(normalizedCall, fieldName);
                        String patternType = ms.getExpression().toString().equals("java.util.regex.Pattern")
                                ? "java.util.regex.Pattern"
                                : "Pattern";
                        int b = utf8.toByteOffset(insertPos);
                        edits.add(makeEdit(
                                b, b,
                                "\n  private static final " + patternType + " " + fieldName + " = " + callSrc + ";\n"));
                    }
                    edits.add(makeEdit(utf8.toByteOffset(start), utf8.toByteOffset(end), fieldName));
                    fix = makeFix("Hoist constant Pattern to `private static final " + fieldName + "`", edits, "Safe");
                } else {
                    fix = makeFix("Hoist constant Pattern to static final field", List.of(), "ManualOnly");
                }
            } else {
                fix = makeFix("Hoist constant Pattern to static final field", List.of(), "ManualOnly");
            }

            String message = isConstant
                    ? (inLoop
                            ? "Constant regex `Pattern.compile(...)` compiled inside loop; hoist to `private static final Pattern`."
                            : "Constant regex `Pattern.compile(...)` compiled per call; hoist to `private static final Pattern`.")
                    : "Dynamic regex `Pattern.compile(...)` compiled in hot path; cache compiled `Pattern` if inputs repeat.";

            diagnostics.add(makeDiag(
                    "JAVA-REGEX-001", "regex-compile-in-loop", "Warning",
                    utf8.toByteOffset(start), utf8.toByteOffset(end),
                    message,
                    proof,
                    fix));
        }

        private String allocatePatternFieldName(ClassTree cls) {
            Set<String> used = usedFieldNamesByClass.computeIfAbsent(cls, c -> {
                Set<String> names = new HashSet<>();
                for (Tree member : c.getMembers()) {
                    if (member instanceof VariableTree vt) {
                        names.add(vt.getName().toString());
                    }
                }
                return names;
            });
            int seq = 1;
            while (used.contains("RIFFTY_PATTERN_" + seq)) {
                seq++;
            }
            String name = "RIFFTY_PATTERN_" + seq;
            used.add(name);
            return name;
        }

        private static boolean isConstantStringLiteral(ExpressionTree expr) {
            if (expr == null)
                return false;
            return switch (expr.getKind()) {
                case STRING_LITERAL -> true;
                case PARENTHESIZED -> isConstantStringLiteral(((ParenthesizedTree) expr).getExpression());
                case PLUS -> {
                    BinaryTree bt = (BinaryTree) expr;
                    yield isConstantStringLiteral(bt.getLeftOperand())
                            && isConstantStringLiteral(bt.getRightOperand());
                }
                default -> false;
            };
        }

        private static boolean isConstantPatternFlags(ExpressionTree expr) {
            if (expr == null)
                return false;
            return switch (expr.getKind()) {
                case INT_LITERAL -> true;
                case PARENTHESIZED -> isConstantPatternFlags(((ParenthesizedTree) expr).getExpression());
                case OR, AND, XOR -> {
                    BinaryTree bt = (BinaryTree) expr;
                    yield isConstantPatternFlags(bt.getLeftOperand())
                            && isConstantPatternFlags(bt.getRightOperand());
                }
                case MEMBER_SELECT -> {
                    MemberSelectTree ms = (MemberSelectTree) expr;
                    String owner = ms.getExpression().toString();
                    yield (owner.equals("Pattern") || owner.equals("java.util.regex.Pattern"))
                            && PATTERN_FLAG_CONSTANTS.contains(ms.getIdentifier().toString());
                }
                case IDENTIFIER -> PATTERN_FLAG_CONSTANTS.contains(expr.toString());
                default -> false;
            };
        }

        private void checkBigDecimal(NewClassTree node, int start, int end) {
            if (node.getArguments().size() != 1)
                return;
            ExpressionTree arg = node.getArguments().getFirst();
            boolean isFloat = arg.getKind() == Tree.Kind.DOUBLE_LITERAL
                    || arg.getKind() == Tree.Kind.FLOAT_LITERAL;
            String resolvedTypeStr = arg.getKind().name();

            if (!isFloat) {
                TreePath argPath = new TreePath(getCurrentPath(), arg);
                TypeMirror tm = trees.getTypeMirror(argPath);
                if (tm != null) {
                    resolvedTypeStr = tm.toString();
                    TypeKind kind = tm.getKind();
                    if (kind == TypeKind.DOUBLE || kind == TypeKind.FLOAT
                            || "java.lang.Double".equals(resolvedTypeStr)
                            || "java.lang.Float".equals(resolvedTypeStr)) {
                        isFloat = true;
                    }
                }
            }
            if (!isFloat || isLineSuppressed(start, "JAVA-BIGDEC-001"))
                return;

            int argStart = (int) positions.getStartPosition(cu, arg);
            int argEnd = (int) positions.getEndPosition(cu, arg);
            if (argStart < 0 || argEnd <= argStart)
                return;
            String argSrc = fi.source().substring(argStart, argEnd);

            Map<String, Object> proof = new LinkedHashMap<>();
            proof.put("tier", "tier2_semantic");
            proof.put("resolved_arg_type", resolvedTypeStr);
            proof.put("arg_expr", argSrc);

            Map<String, Object> fix = makeFix(
                    "Replace `new BigDecimal(" + argSrc + ")` with `BigDecimal.valueOf(" + argSrc + ")`",
                    List.of(makeEdit(utf8.toByteOffset(start), utf8.toByteOffset(end),
                            "BigDecimal.valueOf(" + argSrc + ")")),
                    "Safe");
            diagnostics.add(makeDiag(
                    "JAVA-BIGDEC-001", "bigdecimal-double-literal", "Error",
                    utf8.toByteOffset(start), utf8.toByteOffset(end),
                    "`new BigDecimal(" + argSrc + ")` (" + resolvedTypeStr
                            + ") introduces IEEE-754 rounding error; use `BigDecimal.valueOf(" + argSrc + ")`.",
                    proof,
                    fix));
        }

        private void checkPrimitiveBox(NewClassTree node, String wrapper, int start, int end) {
            if (node.getArguments().size() != 1 || isLineSuppressed(start, "JAVA-BOX-001"))
                return;
            ExpressionTree arg = node.getArguments().getFirst();
            int argStart = (int) positions.getStartPosition(cu, arg);
            int argEnd = (int) positions.getEndPosition(cu, arg);
            if (argStart < 0 || argEnd <= argStart)
                return;
            String argSrc = fi.source().substring(argStart, argEnd);

            Map<String, Object> fix = makeFix(
                    "Replace `new " + wrapper + "(" + argSrc + ")` with `" + wrapper + ".valueOf(" + argSrc + ")`",
                    List.of(makeEdit(utf8.toByteOffset(start), utf8.toByteOffset(end),
                            wrapper + ".valueOf(" + argSrc + ")")),
                    "Safe");
            diagnostics.add(makeDiag(
                    "JAVA-BOX-001", "deprecated-primitive-constructor", "Warning",
                    utf8.toByteOffset(start), utf8.toByteOffset(end),
                    "Deprecated `new " + wrapper + "(" + argSrc + ")`; use `" + wrapper + ".valueOf(" + argSrc + ")`.",
                    Map.of("tier", "tier2_semantic", "wrapper", wrapper),
                    fix));
        }

        private void checkObjectMapper(NewClassTree node, int start, int end) {
            if (!node.getArguments().isEmpty())
                return;
            TreePath path = getCurrentPath();
            MethodTree enclosingMethod = findEnclosingHotMethod(path);
            ClassTree enclosingClass = findEnclosingClass(path);
            if (enclosingMethod == null || enclosingClass == null)
                return;
            if (isLineSuppressed(start, "JAVA-OBJMAP-001"))
                return;

            // Escape & mutation analysis on enclosing method block (RFC §4.7)
            boolean safeToHoist = false;
            int mutatingCalls = 0;
            boolean escapesMethod = false;

            Tree parent = path.getParentPath() != null ? path.getParentPath().getLeaf() : null;
            if (parent instanceof VariableTree varTree && enclosingMethod.getBody() != null) {
                String varName = varTree.getName().toString();
                UsageScanResult usage = scanObjectMapperUsages(enclosingMethod.getBody(), varName);
                mutatingCalls = usage.mutatingCalls;
                escapesMethod = usage.escapes;
                safeToHoist = (mutatingCalls == 0 && !escapesMethod);
            } else if (parent instanceof MemberSelectTree ms) {
                String called = ms.getIdentifier().toString();
                if (!OBJECT_MAPPER_MUTATORS.contains(called)) {
                    safeToHoist = true;
                } else {
                    mutatingCalls = 1;
                }
            }

            Map<String, Object> proof = new LinkedHashMap<>();
            proof.put("tier", "tier2_semantic");
            proof.put("fqn", "com.fasterxml.jackson.databind.ObjectMapper");
            proof.put("mutating_calls", mutatingCalls);
            proof.put("escapes_method", escapesMethod);

            Map<String, Object> fix;
            if (safeToHoist) {
                List<Map<String, Object>> edits = new ArrayList<>();
                boolean alreadyHasField = hasStaticObjectMapperField(enclosingClass);
                if (!alreadyHasField && hoistedFieldScheduled.add(enclosingClass)) {
                    int insertPos = findClassBodyInsertOffset(enclosingClass);
                    if (insertPos >= 0) {
                        int b = utf8.toByteOffset(insertPos);
                        edits.add(makeEdit(
                                b, b,
                                "\n  private static final ObjectMapper RIFFTY_OBJECT_MAPPER = new ObjectMapper();\n"));
                    }
                }
                edits.add(makeEdit(utf8.toByteOffset(start), utf8.toByteOffset(end), "RIFFTY_OBJECT_MAPPER"));
                fix = makeFix("Hoist stateless ObjectMapper to `private static final RIFFTY_OBJECT_MAPPER`", edits,
                        "Safe");
            } else {
                fix = makeFix("Hoist stateless ObjectMapper to `private static final` field", List.of(), "ManualOnly");
            }

            diagnostics.add(makeDiag(
                    "JAVA-OBJMAP-001", "per-call-object-mapper", "Warning",
                    utf8.toByteOffset(start), utf8.toByteOffset(end),
                    "Stateless `new ObjectMapper()` allocated inside method `" + enclosingMethod.getName()
                            + "`; hoist to `private static final` to eliminate per-call reflection/GC burn.",
                    proof,
                    fix));
        }

        private void checkUnpooledHttp(Tree node, String expr, int start, int end) {
            TreePath path = getCurrentPath();
            MethodTree enclosingMethod = findEnclosingHotMethod(path);
            ClassTree enclosingClass = findEnclosingClass(path);
            if (enclosingMethod == null || enclosingClass == null)
                return;
            if (isLineSuppressed(start, "JAVA-HTTP-001"))
                return;

            // Zero-FP guard: do not flag lazy singleton field initialization (`httpClient =
            // new OkHttpClient()`)
            Tree parent = path.getParentPath() != null ? path.getParentPath().getLeaf() : null;
            if (parent instanceof AssignmentTree) {
                return;
            }

            String clientType = expr.contains("OkHttpClient") ? "OkHttpClient" : "HttpClient";
            String fieldName = "RIFFTY_HTTP_CLIENT";
            boolean safeToHoist = parent instanceof VariableTree || parent instanceof MemberSelectTree;

            Map<String, Object> fix;
            if (safeToHoist) {
                List<Map<String, Object>> edits = new ArrayList<>();
                boolean hasField = fi.source().contains(fieldName);
                if (!hasField && hoistedHttpFieldScheduled.add(enclosingClass)) {
                    int insertPos = findClassBodyInsertOffset(enclosingClass);
                    if (insertPos >= 0) {
                        int b = utf8.toByteOffset(insertPos);
                        edits.add(makeEdit(
                                b, b,
                                "\n  private static final " + clientType + " " + fieldName + " = " + expr + ";\n"));
                    }
                }
                edits.add(makeEdit(utf8.toByteOffset(start), utf8.toByteOffset(end), fieldName));
                fix = makeFix("Hoist `" + expr + "` to `private static final " + fieldName + "`", edits, "Safe");
            } else {
                fix = makeFix("Reuse a shared pooled HTTP client instance", List.of(), "ManualOnly");
            }

            diagnostics.add(makeDiag(
                    "JAVA-HTTP-001", "unpooled-http-client", "Warning",
                    utf8.toByteOffset(start), utf8.toByteOffset(end),
                    "`" + expr
                            + "` allocated inside a method body bypasses connection pooling; reuse a shared client instance.",
                    Map.of("tier", "tier2_semantic", "client_expr", expr),
                    fix));
        }

        private void checkUnclosedFilesStream(String method, int start, int end) {
            TreePath p = getCurrentPath().getParentPath();
            ExpressionStatementTree enclosingExprStmt = null;
            while (p != null) {
                Tree leaf = p.getLeaf();
                if (leaf instanceof ReturnTree)
                    return;
                if (leaf instanceof ExpressionStatementTree est && enclosingExprStmt == null) {
                    enclosingExprStmt = est;
                }
                if (leaf instanceof TryTree tt) {
                    for (Tree res : tt.getResources()) {
                        long rs = positions.getStartPosition(cu, res);
                        long re = positions.getEndPosition(cu, res);
                        if (rs <= start && end <= re)
                            return;
                    }
                }
                p = p.getParentPath();
            }
            if (isLineSuppressed(start, "JAVA-LEAK-001"))
                return;

            Map<String, Object> fix;
            if (enclosingExprStmt != null) {
                int stmtStart = (int) positions.getStartPosition(cu, enclosingExprStmt);
                int stmtEnd = (int) positions.getEndPosition(cu, enclosingExprStmt);
                if (stmtStart >= 0 && stmtStart == start && stmtEnd > end) {
                    String filesCall = fi.source().substring(start, end);
                    String tail = fi.source().substring(end, stmtEnd);
                    String indent = lineIndentAt(stmtStart);
                    String replacement = "try (var rifftyStream = " + filesCall + ") {\n"
                            + indent + "    rifftyStream" + tail + "\n"
                            + indent + "}";
                    fix = makeFix(
                            "Wrap `Files." + method + "(...)` in `try-with-resources`",
                            List.of(makeEdit(utf8.toByteOffset(stmtStart), utf8.toByteOffset(stmtEnd), replacement)),
                            "Safe");
                } else {
                    fix = makeFix("Wrap `Files." + method + "(...)` in `try-with-resources`", List.of(), "ManualOnly");
                }
            } else {
                fix = makeFix("Wrap `Files." + method + "(...)` in `try-with-resources`", List.of(), "ManualOnly");
            }

            diagnostics.add(makeDiag(
                    "JAVA-LEAK-001", "unclosed-files-stream", "Error",
                    utf8.toByteOffset(start), utf8.toByteOffset(end),
                    "`Files." + method + "(...)` opens an OS file descriptor stream outside `try-with-resources`.",
                    Map.of("tier", "tier2_semantic", "api", "java.nio.file.Files." + method),
                    fix));
        }

        private String lineIndentAt(int utf16Pos) {
            String src = fi.source();
            int lineStart = src.lastIndexOf('\n', Math.max(0, utf16Pos - 1)) + 1;
            int idx = lineStart;
            while (idx < src.length() && (src.charAt(idx) == ' ' || src.charAt(idx) == '\t')) {
                idx++;
            }
            return src.substring(lineStart, idx);
        }

        private record UsageScanResult(int mutatingCalls, boolean escapes) {
        }

        private static UsageScanResult scanObjectMapperUsages(BlockTree body, String varName) {
            int[] mutators = { 0 };
            boolean[] escapes = { false };
            new TreeScanner<Void, Void>() {
                @Override
                public Void visitReturn(ReturnTree node, Void unused) {
                    if (node.getExpression() != null && node.getExpression().toString().equals(varName)) {
                        escapes[0] = true;
                    }
                    return super.visitReturn(node, unused);
                }

                @Override
                public Void visitAssignment(AssignmentTree node, Void unused) {
                    if (node.getExpression().toString().equals(varName)) {
                        escapes[0] = true;
                    }
                    return super.visitAssignment(node, unused);
                }

                @Override
                public Void visitMethodInvocation(MethodInvocationTree node, Void unused) {
                    for (ExpressionTree arg : node.getArguments()) {
                        if (arg.toString().equals(varName)) {
                            escapes[0] = true;
                        }
                    }
                    if (node.getMethodSelect() instanceof MemberSelectTree ms
                            && ms.getExpression().toString().equals(varName)) {
                        String called = ms.getIdentifier().toString();
                        if (OBJECT_MAPPER_MUTATORS.contains(called) || called.startsWith("set")) {
                            mutators[0]++;
                        }
                    }
                    return super.visitMethodInvocation(node, unused);
                }
            }.scan(body, null);
            return new UsageScanResult(mutators[0], escapes[0]);
        }

        private boolean hasStaticObjectMapperField(ClassTree cls) {
            for (Tree member : cls.getMembers()) {
                if (member instanceof VariableTree vt && vt.getName().contentEquals("RIFFTY_OBJECT_MAPPER")) {
                    return true;
                }
            }
            return false;
        }

        private int findClassBodyInsertOffset(ClassTree cls) {
            int clsStart = (int) positions.getStartPosition(cu, cls);
            int clsEnd = (int) positions.getEndPosition(cu, cls);
            if (clsStart < 0 || clsEnd <= clsStart)
                return -1;
            int braceIdx = fi.source().indexOf('{', clsStart);
            return (braceIdx >= 0 && braceIdx < clsEnd) ? braceIdx + 1 : -1;
        }

        private static MethodTree findEnclosingHotMethod(TreePath path) {
            TreePath cur = path.getParentPath();
            while (cur != null) {
                Tree leaf = cur.getLeaf();
                if (leaf instanceof VariableTree vt && cur.getParentPath() != null
                        && cur.getParentPath().getLeaf() instanceof ClassTree) {
                    return null; // field initializer
                }
                if (leaf instanceof BlockTree bt && bt.isStatic()) {
                    return null; // static initializer
                }
                if (leaf instanceof MethodTree mt) {
                    if (mt.getReturnType() == null)
                        return null; // constructor
                    for (AnnotationTree ann : mt.getModifiers().getAnnotations()) {
                        if (ann.toString().contains("Bean"))
                            return null;
                    }
                    return mt;
                }
                cur = cur.getParentPath();
            }
            return null;
        }

        private static ClassTree findEnclosingClass(TreePath path) {
            TreePath cur = path.getParentPath();
            while (cur != null) {
                if (cur.getLeaf() instanceof ClassTree ct)
                    return ct;
                cur = cur.getParentPath();
            }
            return null;
        }

        private static boolean isInsideLoop(TreePath path) {
            TreePath cur = path.getParentPath();
            while (cur != null) {
                Tree leaf = cur.getLeaf();
                if (leaf instanceof ForLoopTree || leaf instanceof EnhancedForLoopTree
                        || leaf instanceof WhileLoopTree || leaf instanceof DoWhileLoopTree) {
                    return true;
                }
                cur = cur.getParentPath();
            }
            return false;
        }

        private boolean isLineSuppressed(int utf16Pos, String ruleId) {
            String src = fi.source();
            int lineStart = src.lastIndexOf('\n', Math.max(0, utf16Pos - 1)) + 1;
            int lineEnd = src.indexOf('\n', utf16Pos);
            if (lineEnd < 0)
                lineEnd = src.length();
            String line = src.substring(lineStart, lineEnd);
            int idx = line.indexOf("//");
            if (idx >= 0) {
                String c = line.substring(idx + 2).trim();
                if (c.startsWith("riffty:")) {
                    String rest = c.substring("riffty:".length()).trim();
                    if (rest.startsWith("ignore")) {
                        String tail = rest.substring("ignore".length()).trim();
                        if (tail.startsWith("[")) {
                            int close = tail.indexOf(']');
                            if (close > 0) {
                                for (String id : tail.substring(1, close).split(",")) {
                                    if (id.trim().equalsIgnoreCase(ruleId))
                                        return true;
                                }
                            }
                        } else {
                            return true;
                        }
                    }
                }
            }
            return false;
        }

        private Map<String, Object> makeDiag(
                String ruleId, String ruleName, String severity,
                int startByte, int endByte, String message,
                Map<String, Object> typeProof, Map<String, Object> fix) {
            Map<String, Object> range = new LinkedHashMap<>();
            range.put("start", startByte);
            range.put("end", endByte);

            Map<String, Object> d = new LinkedHashMap<>();
            d.put("rule_id", ruleId);
            d.put("rule_name", ruleName);
            d.put("severity", severity);
            d.put("file_id", fi.fileId());
            d.put("range", range);
            d.put("message", message);
            d.put("type_proof", typeProof);
            d.put("fix", fix);
            return d;
        }

        private static Map<String, Object> makeEdit(int startByte, int endByte, String replacement) {
            Map<String, Object> range = new LinkedHashMap<>();
            range.put("start", startByte);
            range.put("end", endByte);
            Map<String, Object> e = new LinkedHashMap<>();
            e.put("range", range);
            e.put("replacement", replacement);
            return e;
        }

        private static Map<String, Object> makeFix(String title, List<Map<String, Object>> edits,
                String applicability) {
            Map<String, Object> f = new LinkedHashMap<>();
            f.put("title", title);
            f.put("edits", edits);
            f.put("required_imports", List.of());
            f.put("applicability", applicability);
            return f;
        }

        static boolean matchesPackageGlob(String candidate, String pattern) {
            if (pattern == null || pattern.isEmpty())
                return false;
            if (pattern.endsWith(".**")) {
                String prefix = pattern.substring(0, pattern.length() - 3);
                return candidate.equals(prefix) || candidate.startsWith(prefix + ".");
            }
            if (pattern.endsWith(".*")) {
                String prefix = pattern.substring(0, pattern.length() - 2);
                if (!candidate.startsWith(prefix + "."))
                    return false;
                String rem = candidate.substring(prefix.length() + 1);
                return !rem.contains(".");
            }
            return candidate.equals(pattern);
        }
    }
}
