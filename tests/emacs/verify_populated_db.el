;;; verify_populated_db.el --- verify org-roam can read a Rust-populated DB  -*- lexical-binding: t -*-

;; Inputs (set via --eval before --load):
;;   org-roam-directory     - directory containing the .org files
;;   org-roam-db-location   - path to the Rust-created org-roam.db
;;   target-node-id         - ID we expect org-roam to find

;; IMPORTANT: this script intentionally does NOT call org-roam-db-sync.
;; The whole point is to check that org-roam can consume the database
;; produced by the Rust populator as-is, without Emacs rebuilding it.

(require 'package)
(package-initialize)
(require 'json)
(require 'org-roam)

;; Disable automatic background sync so nothing modifies the DB.
(when org-roam-db-autosync-mode
  (org-roam-db-autosync-mode -1))

(let ((node (org-roam-node-from-id target-node-id)))
  (if (null node)
      (progn
        (princ (json-encode '(("found" . :json-false))))
        (terpri)
        (kill-emacs 1))
    (princ (json-encode (list (cons "found" t)
                              (cons "id" (org-roam-node-id node))
                              (cons "title" (org-roam-node-title node))
                              (cons "file" (org-roam-node-file node)))))
    (terpri)))
