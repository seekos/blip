package dev.gpui.mobile;

import android.app.Activity;
import android.content.Intent;
import android.database.Cursor;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.Environment;
import android.os.Looper;
import android.provider.DocumentsContract;
import android.provider.OpenableColumns;

import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.util.ArrayList;
import java.util.UUID;
import java.util.concurrent.CountDownLatch;

/** GPUI file prompts use direct shared-storage paths and cache only provider-only documents. */
public final class GpuiPathPicker extends Activity {
    private static final int PICK_FILES = 9010;
    private static Request pending;
    private Request request;

    private static final class Request {
        final CountDownLatch done = new CountDownLatch(1);
        final boolean multiple;
        String[] paths;
        Exception error;

        Request(boolean multiple) { this.multiple = multiple; }

        synchronized void complete(String[] paths, Exception error) {
            if (done.getCount() == 0) return;
            this.paths = paths;
            this.error = error;
            done.countDown();
        }
    }

    /** Called by a Rust worker, never the UI or GPUI event thread. */
    public static String[] openFiles(Activity activity, boolean multiple) throws Exception {
        if (Looper.myLooper() == Looper.getMainLooper()) {
            throw new IllegalStateException("File prompts must run on a worker thread");
        }
        final Request current = new Request(multiple);
        synchronized (GpuiPathPicker.class) {
            if (pending != null) throw new IllegalStateException("A file picker is already open");
            pending = current;
        }
        try {
            activity.runOnUiThread(() -> {
                try {
                    activity.startActivity(new Intent(activity, GpuiPathPicker.class));
                } catch (Exception error) {
                    current.complete(null, error);
                }
            });
            current.done.await();
            if (current.error != null) throw current.error;
            return current.paths;
        } finally {
            synchronized (GpuiPathPicker.class) {
                if (pending == current) pending = null;
            }
        }
    }

    @Override protected void onCreate(Bundle state) {
        super.onCreate(state);
        synchronized (GpuiPathPicker.class) { request = pending; }
        if (request == null) { finish(); return; }
        if (state != null) return;
        Intent intent = new Intent(Intent.ACTION_OPEN_DOCUMENT);
        intent.addCategory(Intent.CATEGORY_OPENABLE);
        intent.setType("*/*");
        intent.putExtra(Intent.EXTRA_ALLOW_MULTIPLE, request.multiple);
        intent.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION);
        try {
            startActivityForResult(intent, PICK_FILES);
        } catch (Exception error) {
            request.complete(null, error);
            finish();
        }
    }

    @Override protected void onActivityResult(int code, int result, Intent data) {
        super.onActivityResult(code, result, data);
        if (code != PICK_FILES || request == null) return;
        if (result != RESULT_OK || data == null) {
            request.complete(null, null);
            finish();
            return;
        }
        ArrayList<Uri> uris = new ArrayList<>();
        if (data.getClipData() != null) {
            for (int i = 0; i < data.getClipData().getItemCount(); i++) {
                Uri uri = data.getClipData().getItemAt(i).getUri();
                if (uri != null) uris.add(uri);
            }
        } else if (data.getData() != null) {
            uris.add(data.getData());
        }
        new Thread(() -> {
            ArrayList<File> selected = new ArrayList<>();
            ArrayList<File> imported = new ArrayList<>();
            try {
                for (Uri uri : uris) {
                    File file = resolveExternalFile(uri);
                    if (file == null) {
                        file = importFile(uri);
                        imported.add(file);
                    }
                    selected.add(file);
                }
                String[] paths = new String[selected.size()];
                for (int i = 0; i < paths.length; i++) paths[i] = selected.get(i).getAbsolutePath();
                request.complete(paths, null);
            } catch (Exception error) {
                for (File file : imported) {
                    file.delete();
                    File parent = file.getParentFile();
                    if (parent != null) parent.delete();
                }
                request.complete(null, error);
            } finally {
                runOnUiThread(this::finish);
            }
        }, "gpui-file-import").start();
    }

    private File resolveExternalFile(Uri uri) throws IOException {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.R
                || !Environment.isExternalStorageManager()
                || !"com.android.externalstorage.documents".equals(uri.getAuthority())
                || !DocumentsContract.isDocumentUri(this, uri)) {
            return null;
        }
        String documentId = DocumentsContract.getDocumentId(uri);
        int separator = documentId.indexOf(':');
        String volume = separator >= 0 ? documentId.substring(0, separator) : documentId;
        String relative = separator >= 0 ? documentId.substring(separator + 1) : "";

        File root;
        if ("primary".equalsIgnoreCase(volume)) {
            root = Environment.getExternalStorageDirectory();
        } else if ("home".equalsIgnoreCase(volume)) {
            root = Environment.getExternalStoragePublicDirectory(Environment.DIRECTORY_DOCUMENTS);
        } else {
            if (volume.isEmpty() || volume.contains("/") || volume.contains("\\")) return null;
            root = new File("/storage", volume);
        }

        File canonicalRoot = root.getCanonicalFile();
        File selected = relative.isEmpty()
                ? canonicalRoot
                : new File(canonicalRoot, relative).getCanonicalFile();
        String rootPath = canonicalRoot.getPath();
        if (!selected.equals(canonicalRoot)
                && !selected.getPath().startsWith(rootPath + File.separator)) {
            throw new SecurityException("Selected file escapes its storage volume");
        }
        return selected.isFile() && selected.canRead() ? selected : null;
    }

    private File importFile(Uri uri) throws IOException {
        String name = null;
        try (Cursor cursor = getContentResolver().query(uri,
                new String[] { OpenableColumns.DISPLAY_NAME }, null, null, null)) {
            if (cursor != null && cursor.moveToFirst()) name = cursor.getString(0);
        }
        if (name == null || name.isEmpty()) name = uri.getLastPathSegment();
        if (name == null) name = "attachment";
        name = name.replace('/', '_').replace('\\', '_').replace('\0', '_');
        if (name.isEmpty() || name.equals(".") || name.equals("..")) name = "attachment";
        File directory = new File(getCacheDir(), "gpui-import-" + UUID.randomUUID());
        if (!directory.mkdir()) throw new IOException("Could not create import directory");
        File file = new File(directory, name);
        try (InputStream input = getContentResolver().openInputStream(uri);
             FileOutputStream output = new FileOutputStream(file)) {
            if (input == null) throw new IOException("Could not read selected document");
            byte[] buffer = new byte[1024 * 1024];
            int count;
            while ((count = input.read(buffer)) != -1) output.write(buffer, 0, count);
        } catch (Exception error) {
            file.delete();
            directory.delete();
            throw error;
        }
        return file;
    }

    @Override protected void onDestroy() {
        if (isFinishing() && request != null) request.complete(null, null);
        super.onDestroy();
    }
}
