package com.amov.blip.mobile;

import android.app.Activity;
import android.content.ActivityNotFoundException;
import android.content.Intent;
import android.database.Cursor;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.Environment;
import android.os.Looper;
import android.provider.DocumentsContract;
import android.provider.Settings;

import java.io.File;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.util.UUID;
import java.util.concurrent.CountDownLatch;

/** Resolves local directory trees directly and imports provider-only trees into cache. */
public final class BlipDirectoryPicker extends Activity {
    private static final int PICK_TREE = 9020;
    private static final int MANAGE_FILES = 9023;
    private static Request pending;
    private Request request;

    private static final class Request {
        final CountDownLatch done = new CountDownLatch(1);
        final boolean shared;
        String path;
        Exception error;

        Request(boolean shared) {
            this.shared = shared;
        }

        synchronized void complete(String path, Exception error) {
            if (done.getCount() == 0) return;
            this.path = path;
            this.error = error;
            done.countDown();
        }
    }

    /** Called by a Rust worker thread. */
    public static String openDirectory(Activity activity) throws Exception {
        return openDirectory(activity, false);
    }

    /** Selects a real shared-storage directory for the built-in SFTP server. */
    public static String openSharedDirectory(Activity activity) throws Exception {
        return openDirectory(activity, true);
    }

    private static String openDirectory(Activity activity, boolean shared) throws Exception {
        if (Looper.myLooper() == Looper.getMainLooper()) {
            throw new IllegalStateException("Directory picker must run on a worker thread");
        }
        Request current = new Request(shared);
        synchronized (BlipDirectoryPicker.class) {
            if (pending != null) throw new IllegalStateException("A directory picker is already open");
            pending = current;
        }
        try {
            activity.runOnUiThread(() -> {
                try {
                    activity.startActivity(new Intent(activity, BlipDirectoryPicker.class));
                } catch (Exception error) {
                    current.complete(null, error);
                }
            });
            current.done.await();
            if (current.error != null) throw current.error;
            return current.path;
        } finally {
            synchronized (BlipDirectoryPicker.class) {
                if (pending == current) pending = null;
            }
        }
    }

    @Override protected void onCreate(Bundle state) {
        super.onCreate(state);
        synchronized (BlipDirectoryPicker.class) { request = pending; }
        if (request == null) { finish(); return; }
        if (state != null) return;
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R
                && !Environment.isExternalStorageManager()) {
            requestAllFilesAccess();
            return;
        }
        openTreePicker();
    }

    private void requestAllFilesAccess() {
        Intent intent = new Intent(
                Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION,
                Uri.parse("package:" + getPackageName()));
        try {
            startActivityForResult(intent, MANAGE_FILES);
        } catch (ActivityNotFoundException missingAppPage) {
            try {
                startActivityForResult(
                        new Intent(Settings.ACTION_MANAGE_ALL_FILES_ACCESS_PERMISSION),
                        MANAGE_FILES);
            } catch (ActivityNotFoundException missingSettingsPage) {
                request.complete(null, missingSettingsPage);
                finish();
            }
        }
    }

    private void openTreePicker() {
        Intent intent = new Intent(Intent.ACTION_OPEN_DOCUMENT_TREE);
        intent.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION
                | Intent.FLAG_GRANT_WRITE_URI_PERMISSION
                | Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION
                | Intent.FLAG_GRANT_PREFIX_URI_PERMISSION);
        try {
            startActivityForResult(intent, PICK_TREE);
        } catch (Exception error) {
            request.complete(null, error);
            finish();
        }
    }

    @Override protected void onActivityResult(int code, int result, Intent data) {
        super.onActivityResult(code, result, data);
        if (code == MANAGE_FILES && request != null) {
            if (Build.VERSION.SDK_INT < Build.VERSION_CODES.R
                    || Environment.isExternalStorageManager()) {
                openTreePicker();
            } else {
                request.complete(null, new SecurityException(
                        "需要允许 Blip 管理所有文件，才能选择手机文件夹"));
                finish();
            }
            return;
        }
        if (code != PICK_TREE || request == null) return;
        if (result != RESULT_OK || data == null || data.getData() == null) {
            request.complete(null, null);
            finish();
            return;
        }
        Uri tree = data.getData();
        if (request.shared) {
            try {
                request.complete(resolveExternalPath(tree, true), null);
            } catch (Exception error) {
                request.complete(null, error);
            }
            finish();
            return;
        }
        if ("com.android.externalstorage.documents".equals(tree.getAuthority())) {
            try {
                request.complete(resolveExternalPath(tree, false), null);
            } catch (Exception error) {
                request.complete(null, error);
            }
            finish();
            return;
        }
        new Thread(() -> {
            File imported = null;
            try {
                String documentId = DocumentsContract.getTreeDocumentId(tree);
                String name = queryName(DocumentsContract.buildDocumentUriUsingTree(tree, documentId));
                if (name == null || name.isEmpty()) name = "folder";
                File parent = new File(getCacheDir(), "blip-tree-" + UUID.randomUUID());
                if (!parent.mkdir()) throw new IllegalStateException("Could not create import directory");
                imported = new File(parent, safeName(name));
                copyDocument(tree, documentId, imported);
                request.complete(imported.getAbsolutePath(), null);
            } catch (Exception error) {
                if (imported != null) deleteRecursively(imported.getParentFile());
                request.complete(null, error);
            } finally {
                runOnUiThread(this::finish);
            }
        }, "blip-directory-import").start();
    }

    private String resolveExternalPath(Uri tree, boolean requireWrite) throws Exception {
        if (!"com.android.externalstorage.documents".equals(tree.getAuthority())) {
            throw new IllegalArgumentException("请选择手机存储或 SD 卡中的目录");
        }
        String documentId = DocumentsContract.getTreeDocumentId(tree);
        int separator = documentId.indexOf(':');
        String volume = separator >= 0 ? documentId.substring(0, separator) : documentId;
        String relative = separator >= 0 ? documentId.substring(separator + 1) : "";

        File root;
        if ("primary".equalsIgnoreCase(volume)) {
            root = Environment.getExternalStorageDirectory();
        } else if ("home".equalsIgnoreCase(volume)) {
            root = Environment.getExternalStoragePublicDirectory(Environment.DIRECTORY_DOCUMENTS);
        } else {
            if (volume.isEmpty() || volume.contains("/") || volume.contains("\\")) {
                throw new IllegalArgumentException("无法识别所选存储设备");
            }
            root = new File("/storage", volume);
        }

        File canonicalRoot = root.getCanonicalFile();
        File selected = relative.isEmpty()
                ? canonicalRoot
                : new File(canonicalRoot, relative).getCanonicalFile();
        String rootPath = canonicalRoot.getPath();
        if (!selected.equals(canonicalRoot)
                && !selected.getPath().startsWith(rootPath + File.separator)) {
            throw new SecurityException("所选目录超出存储设备范围");
        }
        if (!selected.isDirectory()) {
            throw new IllegalArgumentException("所选位置不是可访问的文件夹");
        }
        if (!selected.canRead() || (requireWrite && !selected.canWrite())) {
            throw new SecurityException(requireWrite ? "所选文件夹不可读写" : "所选文件夹不可读取");
        }
        return selected.getAbsolutePath();
    }

    private void copyDocument(Uri tree, String id, File target) throws Exception {
        Uri document = DocumentsContract.buildDocumentUriUsingTree(tree, id);
        String mime = query(document, DocumentsContract.Document.COLUMN_MIME_TYPE);
        if (DocumentsContract.Document.MIME_TYPE_DIR.equals(mime)) {
            if (!target.mkdir()) throw new IllegalStateException("Could not create " + target);
            Uri children = DocumentsContract.buildChildDocumentsUriUsingTree(tree, id);
            try (Cursor cursor = getContentResolver().query(children,
                    new String[] {
                        DocumentsContract.Document.COLUMN_DOCUMENT_ID,
                        DocumentsContract.Document.COLUMN_DISPLAY_NAME,
                    }, null, null, null)) {
                if (cursor == null) return;
                while (cursor.moveToNext()) {
                    String childId = cursor.getString(0);
                    String childName = safeName(cursor.getString(1));
                    copyDocument(tree, childId, new File(target, childName));
                }
            }
        } else {
            try (InputStream input = getContentResolver().openInputStream(document);
                 FileOutputStream output = new FileOutputStream(target)) {
                if (input == null) throw new IllegalStateException("Could not read " + document);
                byte[] buffer = new byte[1024 * 1024];
                int count;
                while ((count = input.read(buffer)) != -1) output.write(buffer, 0, count);
            }
        }
    }

    private String queryName(Uri document) {
        return query(document, DocumentsContract.Document.COLUMN_DISPLAY_NAME);
    }

    private String query(Uri document, String column) {
        try (Cursor cursor = getContentResolver().query(document,
                new String[] { column }, null, null, null)) {
            return cursor != null && cursor.moveToFirst() ? cursor.getString(0) : null;
        }
    }

    private static String safeName(String name) {
        if (name == null) return "item";
        name = name.replace('/', '_').replace('\\', '_').replace('\0', '_');
        return name.isEmpty() || name.equals(".") || name.equals("..") ? "item" : name;
    }

    private static void deleteRecursively(File file) {
        if (file == null) return;
        File[] children = file.listFiles();
        if (children != null) for (File child : children) deleteRecursively(child);
        file.delete();
    }
}

