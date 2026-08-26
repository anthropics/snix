# Patches that the Lix core team developed for git.lix.systems
# Re-applied for git.snix.dev
{ ... }:
{
  patches = {
    # Sign in redirection to the OAuth 2 handler.
    signin_redirect = ./signin-redirect.patch;
    # Series to make Forgejo more Gerrit compatible.
    forgejo_is_now_gerrit_native = ./0001-lix-Make-a-Code-Review-Gerrit-tab.patch;
    # Modified for our Gerrit instance.
    forgejo_knows_about_gerrit = ./0002-lix-link-gerrit-cl-and-change-ids.patch;
  };
  custom_emojis = ./emojis.txt;
}
